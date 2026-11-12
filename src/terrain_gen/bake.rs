//! A baked map (height grids, roads, terrain textures) and its file format, so
//! the web build can download the built-in map instead of spending ~15 s
//! generating it on one thread. `scripts/build-web.sh` writes the file with
//! `FLIGHT_SIM_BAKE_MAP=<path> cargo run --release`.
//!
//! Format: `FSMAP1` then a deflated body. Heights are stored as u16 decimetres
//! and the textures as bytes, both delta-coded along rows so they compress.

use bevy::{
    asset::{AssetLoader, LoadContext, io::Reader},
    prelude::*,
};

use super::{HeightGrid, TextureBake, roads::Road};

const MAGIC: &[u8] = b"FSMAP1";
const HEIGHT_OFFSET: f32 = 1000.0;
const HEIGHT_STEP: f32 = 0.1;

pub struct MapBake {
    pub seed: u32,
    pub(super) near: HeightGrid,
    pub(super) fine: HeightGrid,
    pub(super) coarse: HeightGrid,
    pub(super) roads: Vec<Road>,
    pub(super) textures: [TextureBake; 3],
}

impl MapBake {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.u32(self.seed);
        for g in [&self.near, &self.fine, &self.coarse] {
            w.f32(g.origin.x);
            w.f32(g.origin.y);
            w.f32(g.spacing);
            w.u32(g.n as u32);
            let q: Vec<u16> = g
                .heights
                .iter()
                .map(|h| ((h + HEIGHT_OFFSET) / HEIGHT_STEP).round().clamp(0.0, 65535.0) as u16)
                .collect();
            for (k, &v) in q.iter().enumerate() {
                w.u16(v.wrapping_sub(predict(&q, k, g.n, 1)));
            }
        }
        w.u32(self.roads.len() as u32);
        for road in &self.roads {
            w.u32(road.len() as u32);
            for p in road {
                w.f32(p.x);
                w.f32(p.y);
                w.f32(p.z);
            }
        }
        for t in &self.textures {
            w.u32(t.res as u32);
            // 6 bits a channel is plenty for noisy terrain and compresses far better
            let coarse = |v: &[u8]| -> Vec<u8> { v.iter().map(|b| (b & 0xFC) | 0x02).collect() };
            w.0.extend(delta(&coarse(&t.color), t.res, 3));
            w.0.extend(delta(&coarse(&t.normal), t.res, 2));
        }
        let mut out = MAGIC.to_vec();
        out.extend(miniz_oxide::deflate::compress_to_vec(&w.0, 7));
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<MapBake, String> {
        let body = bytes.strip_prefix(MAGIC).ok_or("not a map file")?;
        let body = miniz_oxide::inflate::decompress_to_vec(body).map_err(|e| format!("{e:?}"))?;
        let mut r = Reader_ { b: &body, at: 0 };
        let seed = r.u32()?;
        let mut grid = || -> Result<HeightGrid, String> {
            let origin = Vec2::new(r.f32()?, r.f32()?);
            let spacing = r.f32()?;
            let n = r.u32()? as usize;
            let mut q: Vec<u16> = Vec::with_capacity(n * n);
            for k in 0..n * n {
                let p = predict(&q, k, n, 1);
                q.push(p.wrapping_add(r.u16()?));
            }
            let heights = q.iter().map(|&v| v as f32 * HEIGHT_STEP - HEIGHT_OFFSET).collect();
            Ok(HeightGrid { origin, spacing, inv_spacing: 1.0 / spacing, n, heights })
        };
        let (near, fine, coarse) = (grid()?, grid()?, grid()?);
        let mut roads = Vec::new();
        for _ in 0..r.u32()? {
            let len = r.u32()? as usize;
            let mut road = Vec::with_capacity(len);
            for _ in 0..len {
                road.push(Vec3::new(r.f32()?, r.f32()?, r.f32()?));
            }
            roads.push(road);
        }
        let mut texture = || -> Result<TextureBake, String> {
            let res = r.u32()? as usize;
            let color = undelta(r.take(res * res * 3)?, res, 3);
            let normal = undelta(r.take(res * res * 2)?, res, 2);
            Ok(TextureBake { res, color, normal })
        };
        let textures = [texture()?, texture()?, texture()?];
        Ok(MapBake { seed, near, fine, coarse, roads, textures })
    }
}

/// Gradient predictor (left + up - up-left, as in PNG's Paeth family) for the
/// value at `k` of a row-major grid `width` values wide with `channels`
/// interleaved channels; only reads values before `k`.
fn predict<T: Copy + Into<i64> + TryFrom<i64> + Default>(v: &[T], k: usize, width: usize, channels: usize) -> T {
    let row = width * channels;
    let (x, y) = (k % row, k / row);
    let get = |i: usize| -> i64 { v[i].into() };
    let p = match (x >= channels, y > 0) {
        (false, false) => 0,
        (true, false) => get(k - channels),
        (false, true) => get(k - row),
        (true, true) => get(k - channels) + get(k - row) - get(k - row - channels),
    };
    T::try_from(p).unwrap_or_else(|_| {
        // out of range for the type: fall back to the left neighbour
        if x >= channels { v[k - channels] } else { T::default() }
    })
}

fn delta(data: &[u8], res: usize, channels: usize) -> Vec<u8> {
    (0..data.len()).map(|k| data[k].wrapping_sub(predict(data, k, res, channels))).collect()
}

fn undelta(data: &[u8], res: usize, channels: usize) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(data.len());
    for (k, &d) in data.iter().enumerate() {
        let p = predict(&out, k, res, channels);
        out.push(p.wrapping_add(d));
    }
    out
}

struct Writer(Vec<u8>);

impl Writer {
    fn u16(&mut self, v: u16) {
        self.0.extend(v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend(v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.0.extend(v.to_le_bytes());
    }
}

struct Reader_<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader_<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let s = self.b.get(self.at..self.at + n).ok_or("map file is truncated")?;
        self.at += n;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn f32(&mut self) -> Result<f32, String> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
}

/// The raw bytes of a `.map` file; decoded with `MapBake::from_bytes`.
#[derive(Asset, TypePath)]
pub struct MapFile(pub Vec<u8>);

#[derive(Default, TypePath)]
pub struct MapFileLoader;

impl AssetLoader for MapFileLoader {
    type Asset = MapFile;
    type Settings = ();
    type Error = std::io::Error;

    async fn load(
        &self,
        reader: &mut dyn Reader,
        _settings: &(),
        _load_context: &mut LoadContext<'_>,
    ) -> Result<MapFile, Self::Error> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        Ok(MapFile(bytes))
    }

    fn extensions(&self) -> &[&str] {
        &["map"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_round_trips() {
        let data: Vec<u8> = (0..4 * 4 * 3).map(|i| (i * 37 % 251) as u8).collect();
        assert_eq!(undelta(&delta(&data, 4, 3), 4, 3), data);
    }
}
