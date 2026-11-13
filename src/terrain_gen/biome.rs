//! Biome parameter sets: everything the bake, the trees and the terrain shader
//! do differently per biome (height shaping, ground palette, tree cover, snow,
//! fields). The biome itself is chosen with the weather (`environment::Biome`);
//! the map's biome is a global like the seed, so `sample_terrain` (also used
//! at runtime by the tree planter) can see it.

use std::sync::{
    LazyLock,
    atomic::{AtomicU8, Ordering},
};

use bevy::prelude::*;

pub use crate::environment::Biome;

use super::{smoothstep, srgb};

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// Make `biome` the map's biome (`MapBaker::new` / `spawn_map`).
pub(super) fn set(biome: Biome) {
    CURRENT.store(biome as u8, Ordering::Relaxed);
}

/// The current map's biome parameters.
#[inline]
pub(super) fn params() -> &'static BiomeParams {
    &PARAMS[CURRENT.load(Ordering::Relaxed) as usize]
}

/// Colours are linear RGB (authored as sRGB below).
pub(super) struct BiomeParams {
    // ---- height shaping (`sample_terrain`)
    /// Multiplier on the height above y = 0 (lower, gentler hills below 1).
    pub relief: f32,
    /// Ground below `soft_floor` (m) is squashed into broad, nearly flat floors
    /// that bottom out about `soft_floor_range` lower.
    pub soft_floor: f32,
    pub soft_floor_range: f32,
    /// Islands (archipelago): the ground is dropped by this much (m) so about half the
    /// map is under the sea, with the relief varying region to region and gentle
    /// shelves at the waterline. 0 = off.
    pub sea_shift: f32,

    // ---- water (`water`)
    pub water: Option<Water>,

    // ---- ground (`terrain_color` / `cover`)
    pub meadow: Vec3,
    pub dry_meadow: Vec3,
    pub forest: Vec3,
    /// Above the tree line.
    pub alpine: Vec3,
    pub rock: Vec3,
    pub dark_rock: Vec3,
    pub snow: Vec3,
    pub tree_line: f32,
    pub snow_line: f32,
    /// Snow lies where the ground's flatness (normal.y) is above this range.
    pub snow_flatness: (f32, f32),
    /// Added to the slope where rock shows through (more rock above 0).
    pub rock_bias: f32,
    /// Patchy snow on gentle ground below the snow line (0..1).
    pub low_snow: f32,
    /// How much snow still shows inside forests (1 = forests don't hide it).
    pub snow_in_forest: f32,
    /// Woods on the flat valley floors (patches), relative to the original.
    pub valley_woods: f32,
    /// Slopes are wooded where the flatness (normal.y) drops through this range.
    pub slope_woods: (f32, f32),
    /// Woods on rounded hilltops / ridges.
    pub hilltop_woods: f32,
    /// A patchwork of fields on flat low ground (see `fields`), below this height.
    pub field_line: Option<f32>,

    // ---- trees (`trees::plant_one`)
    /// Multiplier on the tree cover.
    pub tree_density: f32,
    pub tree_scale: f32,
    /// Share of conifers on the valley floors and high up.
    pub conifers: (f32, f32),
    pub conifer: Vec3,
    pub broadleaf: Vec3,
    /// Broadleaves turning early, and how many.
    pub autumn: Vec3,
    pub autumn_share: f32,
    /// Snow on the crowns (0..1).
    pub tree_snow: f32,

    // ---- terrain shader (`TerrainParams::biome`)
    /// Tint of the per-pixel rock colour (multiplies the shader's grey-brown).
    pub shader_rock: Vec3,
    /// Strength of the per-pixel vegetation mottling (the original is 1).
    pub veg_patches: f32,
    /// x..y: the per-pixel snow fades out over this slope (1 - normal.y) range,
    /// z: 1 makes it follow the baked colour (no fresh snow on forest or rock).
    pub shader_snow: Vec3,
}

/// Lakes or the sea: a single water level per map (see `water`).
pub(super) struct Water {
    /// Height of the water surface (m).
    pub level: f32,
    /// The sea: the water carries on past the map to the horizon.
    pub sea: bool,
    /// Frozen over (pale, rough ice: no ripples or glint).
    pub frozen: bool,
    /// Beach / shore band above the water (m), coloured `sand`.
    pub beach: f32,
    pub sand: Vec3,
    /// Water colour over the shallows and the deep (linear), and the depth (m) it takes
    /// to go from one to the other.
    pub shallow: Vec3,
    pub deep: Vec3,
    pub depth_scale: f32,
    /// Width (m of depth) of the foam line at the shore.
    pub foam: f32,
    /// Strength of the sky reflection (lakes sit among dark mountains: less).
    pub reflect: f32,
}

static PARAMS: LazyLock<[BiomeParams; 4]> = LazyLock::new(|| [alpine(), tundra(), farmland(), archipelago()]);

/// Mountain lakes: a level just above the soft floor's flats, so the deepest basins fill.
fn lakes(level: f32) -> Water {
    Water {
        level,
        sea: false,
        frozen: false,
        beach: 1.5,
        sand: srgb(0.45, 0.42, 0.36),
        shallow: srgb(0.16, 0.30, 0.26),
        deep: srgb(0.02, 0.07, 0.09),
        depth_scale: 9.0,
        foam: 0.0,
        // (mountains all round: the sky is only part of what a lake reflects)
        reflect: 0.5,
    }
}

fn alpine() -> BiomeParams {
    BiomeParams {
        relief: 1.0,
        soft_floor: 120.0,
        soft_floor_range: 180.0,
        sea_shift: 0.0,
        water: Some(lakes(LAKE_LEVEL)),
        meadow: srgb(0.42, 0.47, 0.22),
        dry_meadow: srgb(0.52, 0.50, 0.30),
        forest: srgb(0.17, 0.25, 0.11),
        alpine: srgb(0.46, 0.44, 0.30),
        rock: srgb(0.52, 0.46, 0.36),
        dark_rock: srgb(0.31, 0.27, 0.22),
        snow: srgb(0.94, 0.95, 0.98),
        tree_line: 1650.0,
        snow_line: 2350.0,
        snow_flatness: (0.55, 0.72),
        rock_bias: 0.0,
        low_snow: 0.0,
        snow_in_forest: 1.0,
        valley_woods: 1.0,
        slope_woods: (0.985, 0.94),
        hilltop_woods: 0.0,
        field_line: None,
        tree_density: 1.0,
        tree_scale: 1.0,
        conifers: (0.15, 1.0),
        conifer: srgb(0.15, 0.22, 0.11),
        broadleaf: srgb(0.22, 0.31, 0.12),
        autumn: srgb(0.48, 0.38, 0.12),
        autumn_share: 0.05,
        tree_snow: 0.0,
        shader_rock: Vec3::ONE,
        veg_patches: 1.0,
        shader_snow: Vec3::new(0.55, 0.75, 0.0),
    }
}

fn tundra() -> BiomeParams {
    BiomeParams {
        relief: 1.0,
        soft_floor: 120.0,
        soft_floor_range: 180.0,
        sea_shift: 0.0,
        water: Some(Water {
            frozen: true,
            shallow: srgb(0.80, 0.86, 0.90),
            deep: srgb(0.32, 0.44, 0.54),
            sand: srgb(0.62, 0.62, 0.60),
            ..lakes(LAKE_LEVEL)
        }),
        // frozen, bleached tundra grass where the snow has blown off
        meadow: srgb(0.50, 0.48, 0.40),
        dry_meadow: srgb(0.58, 0.55, 0.47),
        forest: srgb(0.12, 0.16, 0.13),
        alpine: srgb(0.62, 0.63, 0.62),
        // pale blue-grey rock
        rock: srgb(0.50, 0.52, 0.55),
        dark_rock: srgb(0.27, 0.29, 0.32),
        snow: srgb(0.92, 0.94, 0.98),
        tree_line: 1150.0,
        snow_line: 380.0,
        // wind-scoured: the steeper slopes show their rock
        snow_flatness: (0.74, 0.86),
        rock_bias: 0.06,
        low_snow: 0.6,
        snow_in_forest: 0.6,
        valley_woods: 0.6,
        slope_woods: (0.985, 0.94),
        hilltop_woods: 0.0,
        field_line: None,
        tree_density: 0.4,
        tree_scale: 0.85,
        conifers: (1.0, 1.0),
        conifer: srgb(0.10, 0.15, 0.11),
        broadleaf: srgb(0.30, 0.27, 0.22),
        autumn: srgb(0.30, 0.27, 0.22),
        autumn_share: 0.0,
        tree_snow: 0.75,
        shader_rock: Vec3::new(0.95, 1.02, 1.12),
        veg_patches: 0.7,
        shader_snow: Vec3::new(0.3, 0.45, 1.0),
    }
}

fn farmland() -> BiomeParams {
    BiomeParams {
        // rolling hills: half the relief and broad flat plains
        relief: 0.5,
        soft_floor: 260.0,
        soft_floor_range: 380.0,
        sea_shift: 0.0,
        // (the plains bottom out lower)
        water: Some(Water { shallow: srgb(0.20, 0.30, 0.22), ..lakes(FARM_LAKE_LEVEL) }),
        meadow: srgb(0.38, 0.48, 0.20),
        dry_meadow: srgb(0.48, 0.50, 0.26),
        forest: srgb(0.15, 0.24, 0.10),
        // moorland on the tops: heather and bracken
        alpine: srgb(0.42, 0.36, 0.28),
        rock: srgb(0.50, 0.46, 0.38),
        dark_rock: srgb(0.30, 0.27, 0.23),
        snow: srgb(0.94, 0.95, 0.98),
        tree_line: 1250.0,
        snow_line: 5000.0,
        snow_flatness: (0.55, 0.72),
        rock_bias: 0.0,
        low_snow: 0.0,
        snow_in_forest: 1.0,
        valley_woods: 0.35,
        // pasture on the gentle slopes, hanging woods on the steep ones
        slope_woods: (0.95, 0.89),
        hilltop_woods: 0.6,
        field_line: Some(650.0),
        tree_density: 0.9,
        tree_scale: 1.0,
        conifers: (0.05, 0.4),
        conifer: srgb(0.14, 0.21, 0.11),
        broadleaf: srgb(0.21, 0.31, 0.11),
        autumn: srgb(0.42, 0.40, 0.14),
        autumn_share: 0.04,
        tree_snow: 0.0,
        shader_rock: Vec3::ONE,
        veg_patches: 0.45,
        shader_snow: Vec3::new(0.55, 0.75, 0.0),
    }
}

/// Lake level in the mountains, just above the deepest flats (which bottom out near -60 m).
const LAKE_LEVEL: f32 = 20.0;
const FARM_LAKE_LEVEL: f32 = 55.0;

fn archipelago() -> BiomeParams {
    BiomeParams {
        relief: 1.0,
        // a flat sea bed far down
        soft_floor: -260.0,
        soft_floor_range: 240.0,
        sea_shift: 1350.0,
        water: Some(Water {
            level: 0.0,
            sea: true,
            frozen: false,
            beach: 6.0,
            sand: srgb(0.86, 0.79, 0.62),
            shallow: srgb(0.18, 0.62, 0.60),
            deep: srgb(0.02, 0.15, 0.30),
            depth_scale: 26.0,
            foam: 1.1,
            reflect: 1.0,
        }),
        // lush, sunlit greens; scrub and bare volcanic rock up high
        meadow: srgb(0.36, 0.50, 0.19),
        dry_meadow: srgb(0.54, 0.53, 0.29),
        forest: srgb(0.10, 0.24, 0.08),
        alpine: srgb(0.44, 0.42, 0.28),
        rock: srgb(0.44, 0.39, 0.33),
        dark_rock: srgb(0.24, 0.21, 0.19),
        snow: srgb(0.94, 0.95, 0.98),
        tree_line: 1250.0,
        snow_line: 9000.0,
        snow_flatness: (0.55, 0.72),
        rock_bias: 0.0,
        low_snow: 0.0,
        snow_in_forest: 1.0,
        valley_woods: 0.9,
        slope_woods: (0.99, 0.95),
        hilltop_woods: 0.4,
        field_line: None,
        tree_density: 1.0,
        tree_scale: 0.95,
        conifers: (0.0, 0.25),
        conifer: srgb(0.12, 0.22, 0.10),
        broadleaf: srgb(0.17, 0.33, 0.10),
        autumn: srgb(0.30, 0.36, 0.10),
        autumn_share: 0.06,
        tree_snow: 0.0,
        shader_rock: Vec3::new(0.82, 0.8, 0.8),
        veg_patches: 0.6,
        shader_snow: Vec3::new(0.55, 0.75, 0.0),
    }
}

/// How the ground is covered at a point (each 0..1), shared by the colour
/// bake and the tree planter so the trees grow where the ground looks wooded.
pub(super) struct Cover {
    pub forest: f32,
    /// Fields (farmland), and hedge coverage along their edges.
    pub field: f32,
    pub alpine: f32,
    pub rock: f32,
    pub snow: f32,
    /// Beach / lake shore, and the lake or sea bed below the water.
    pub sand: f32,
    /// 0 under water and on the shore, 1 inland: no trees or fields below.
    pub dry: f32,
}

/// `field`: the field at the point, where `may_have_fields` (farmland only).
pub(super) fn cover(b: &BiomeParams, h: f32, normal: Vec3, ridge: f32, jitter: f32, field: Option<&super::fields::Field>) -> Cover {
    let flatness = normal.y; // 1 = flat
    let ridge = ridge.clamp(-1.0, 1.0);
    let crease = (-ridge).max(0.0);

    // Valley floors are a patchwork of meadow and woods, slopes are forested up to the tree
    // line (which reaches higher up the gullies, where the drainage is), then alpine grass.
    let tree_line = b.tree_line + jitter * 250.0 + crease * 250.0 - ridge.max(0.0) * 200.0;
    let below_tree_line = smoothstep(tree_line + 150.0, tree_line - 150.0, h);
    let woods = smoothstep(0.15, 0.45, jitter + crease * 0.3) * b.valley_woods;
    let slopes = smoothstep(b.slope_woods.0, b.slope_woods.1 - crease * 0.05, flatness);
    let hilltops = b.hilltop_woods * smoothstep(0.2, 0.55, ridge + jitter * 0.3);
    let mut forest = slopes.max(woods).max(hilltops) * below_tree_line;

    // the shore: a beach on gentle ground, nothing grows below it
    let (mut sand, mut dry, mut coast) = (0.0, 1.0, 0.0);
    if let Some(w) = &b.water {
        let above = h - w.level + jitter * 1.5;
        dry = smoothstep(w.beach * 0.6, w.beach + 3.0, above);
        sand = smoothstep(w.beach + 1.0, w.beach - 1.0, above) * smoothstep(0.80, 0.93, flatness);
        sand = sand.max(smoothstep(0.5, -0.5, above));
        if w.sea {
            // sea cliffs: bare rock on the steep coasts
            coast = smoothstep(120.0, 20.0, above);
            // dune grass behind the beaches before the woods start
            dry = smoothstep(w.beach + 2.0, w.beach + 12.0, above);
        }
        forest *= dry;
    }

    let mut field_amt = 0.0;
    if let (Some(f), Some(line)) = (field, b.field_line) {
        let amt = fields_mask(line, h, flatness, jitter);
        if f.wood {
            forest = forest.max(amt * dry);
        } else {
            field_amt = amt * dry;
            forest *= 1.0 - field_amt;
        }
    }
    let alpine = smoothstep(tree_line - 100.0, tree_line + 250.0, h);

    // Rock on steep slopes, more exposed on ridges and higher up.
    let steep_start = 0.70 + b.rock_bias + coast * 0.06 + ridge * 0.06 + jitter * 0.04 + smoothstep(1500.0, 3000.0, h) * 0.08;
    let rock = smoothstep(steep_start, steep_start - 0.10, flatness);

    // Snow on high, not-too-steep ground; it lingers longer in creases.
    let snow_line = b.snow_line + jitter * 200.0 - crease * 200.0;
    let mut snow = smoothstep(snow_line - 100.0, snow_line + 250.0, h)
        * smoothstep(
            b.snow_flatness.0 - smoothstep(snow_line + 150.0, snow_line + 1150.0, h) * 0.15,
            b.snow_flatness.1,
            flatness,
        );
    if b.low_snow > 0.0 {
        // drifts on the low ground, blown off the bumps
        let drifts = smoothstep(-0.3, 0.1, jitter + crease * 0.4) * smoothstep(0.8, 0.9, flatness);
        snow = snow.max(drifts * b.low_snow);
    }
    snow *= 1.0 - forest * (1.0 - b.snow_in_forest);
    Cover { forest, field: field_amt, alpine, rock, snow, sand, dry }
}

/// Where fields go: flat, low ground (0..1).
#[inline]
pub(super) fn fields_mask(line: f32, h: f32, flatness: f32, jitter: f32) -> f32 {
    let line = line + jitter * 120.0;
    smoothstep(0.962, 0.982, flatness) * smoothstep(line + 120.0, line - 120.0, h)
}

/// Whether a point could hold fields at all (to skip the field lookup).
#[inline]
pub(super) fn may_have_fields(b: &BiomeParams, h: f32, flatness: f32) -> bool {
    b.field_line.is_some_and(|line| flatness > 0.962 && h < line + 250.0)
}
