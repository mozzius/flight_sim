//! Lakes and the sea: one flat water plane per map at the biome's water level
//! (`biome::Water`), drawn wherever the ground is below it (the depth test does
//! the clipping, and the depth prepass means only the visible water is shaded).
//!
//! The shader (`assets/shaders/water.wesl`) colours the water by its depth,
//! from shallow turquoise to deep blue, adds a Fresnel reflection of the sky
//! (the `Look`'s horizon and zenith colours), a sun glint, animated ripple
//! normals at a few scales (faded out once smaller than a pixel) and foam at the
//! shore. The depth comes from three small R8 textures baked from the height
//! grids (one texel per grid vertex), so it lines up with the terrain mesh.
//! Tundra lakes are frozen: pale, cracked ice with snow patches and no glint.
//!
//! The sea (archipelago) follows the camera out to the horizon; lakes cover the map.

use bevy::{
    asset::RenderAssetUsages,
    image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor},
    pbr::{ExtendedMaterial, MaterialExtension},
    prelude::*,
    render::render_resource::{AsBindGroup, Extent3d, ShaderType, TextureDimension, TextureFormat},
};

use super::{HeightGrid, biome};

pub type WaterMaterial = ExtendedMaterial<StandardMaterial, WaterExt>;

#[derive(Asset, AsBindGroup, Reflect, Debug, Clone, Default)]
pub struct WaterExt {
    #[uniform(100)]
    pub params: WaterParams,
    /// Depth below the water at each vertex of the near / fine / coarse height grid
    /// (signed, square-root encoded: see `DEPTH_RANGE`).
    #[texture(101)]
    #[sampler(104)]
    pub near: Handle<Image>,
    #[texture(102)]
    pub fine: Handle<Image>,
    #[texture(103)]
    pub coarse: Handle<Image>,
}

#[derive(ShaderType, Reflect, Debug, Clone, Default)]
pub struct WaterParams {
    /// rgb: shallow colour (linear), a: depth (m) to reach the deep colour
    pub shallow: Vec4,
    /// rgb: deep colour, a: foam width (m of depth)
    pub deep: Vec4,
    /// rgb: sky colour at the horizon (post-exposure, linear), a: reflection strength
    pub horizon: Vec4,
    /// rgb: sky colour overhead, a: 1 = frozen
    pub zenith: Vec4,
    /// rgb: sun glint colour and strength
    pub glint: Vec4,
    /// xyz: direction to the sun, w: water level (m)
    pub sun: Vec4,
    /// rgb: mist colour, a: mist density multiplier (as the terrain's)
    pub mist: Vec4,
    /// x: mist brightness, y: ripple strength, z: ripple octaves,
    /// w: radius of the atmosphere's planet (0 = no atmosphere; see `water_prepass.wesl`)
    pub params: Vec4,
    /// xy: world position of the near / fine / coarse depth texture's corner, z: 1 / its extent
    pub near_frame: Vec4,
    pub fine_frame: Vec4,
    pub coarse_frame: Vec4,
}

impl MaterialExtension for WaterExt {
    fn fragment_shader() -> bevy::shader::ShaderRef {
        "shaders/water.wesl".into()
    }

    // cuts the water off at the atmosphere planet's horizon (needs `AlphaMode::Mask`
    // for the prepass to run a fragment shader at all)
    fn prepass_fragment_shader() -> bevy::shader::ShaderRef {
        "shaders/water_prepass.wesl".into()
    }
}

/// Keeps the sea plane under the camera (it reaches the far plane all round).
#[derive(Component)]
pub struct FollowCamera;

/// `Atmosphere::earth`'s planet (centred this far below the origin).
const PLANET_RADIUS: f32 = 6_360_000.0;
/// Depth range of the textures (m, either side of the surface).
const DEPTH_RANGE: f32 = 128.0;
/// Radius of the sea: the projection has no far plane, and anything short of
/// "almost the horizon" leaves a strip of sky below it from up high.
const SEA_RADIUS: f32 = 2_000_000.0;

/// One texel per grid vertex: the signed depth below `level`, square-root encoded
/// (fine steps in the shallows, where the colour changes fastest).
fn depth_image(grid: &HeightGrid, level: f32) -> (Image, Vec4) {
    let n = grid.n;
    let data: Vec<u8> = grid
        .heights
        .iter()
        .map(|&h| {
            let d = ((level - h) / DEPTH_RANGE).clamp(-1.0, 1.0);
            let e = d.signum() * d.abs().sqrt();
            ((e * 0.5 + 0.5) * 255.0 + 0.5) as u8
        })
        .collect();
    let mut image = Image::new(
        Extent3d { width: n as u32, height: n as u32, depth_or_array_layers: 1 },
        TextureDimension::D2,
        data,
        TextureFormat::R8Unorm,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::ClampToEdge,
        address_mode_v: ImageAddressMode::ClampToEdge,
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        ..default()
    });
    // texel centres on the vertices
    let corner = grid.origin - Vec2::splat(grid.spacing * 0.5);
    let frame = Vec4::new(corner.x, corner.y, 1.0 / (grid.spacing * n as f32), 0.0);
    (image, frame)
}

/// sRGB-authored display colour to linear.
fn lin(v: Vec3) -> Vec3 {
    let c = LinearRgba::from(Color::srgb(v.x, v.y, v.z));
    Vec3::new(c.red, c.green, c.blue)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn spawn(
    w: &biome::Water,
    look: &crate::environment::Look,
    [near, fine, coarse]: [&HeightGrid; 3],
    low_memory: bool,
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<WaterMaterial>,
    images: &mut Assets<Image>,
) -> Entity {
    let (fine_image, fine_frame) = depth_image(fine, w.level);
    let fine_handle = images.add(fine_image);
    // low-memory devices draw the 64 m terrain mesh in the middle: match it
    let (near_handle, near_frame) = if low_memory {
        (fine_handle.clone(), fine_frame)
    } else {
        let (image, frame) = depth_image(near, w.level);
        (images.add(image), frame)
    };
    let (coarse_image, coarse_frame) = depth_image(coarse, w.level);

    // the sky's colours, as the fallback dome draws them (unlit, linear)
    let horizon = lin(look.sky_horizon);
    let zenith = lin(look.sky_zenith);
    let material = materials.add(ExtendedMaterial {
        base: StandardMaterial {
            base_color: Color::WHITE,
            perceptual_roughness: 0.6,
            // the reflections are the shader's own
            reflectance: 0.0,
            alpha_mode: AlphaMode::Mask(0.5),
            ..default()
        },
        extension: WaterExt {
            params: WaterParams {
                shallow: w.shallow.extend(w.depth_scale),
                deep: w.deep.extend(w.foam),
                horizon: horizon.extend(w.reflect),
                zenith: zenith.extend(if w.frozen { 1.0 } else { 0.0 }),
                glint: look.glint.extend(0.0),
                sun: look.sun_dir.extend(w.level),
                mist: look.mist,
                params: Vec4::new(
                    1.0,
                    if w.sea { 1.0 } else { 0.6 },
                    if low_memory { 4.0 } else { 5.0 },
                    if crate::platform::atmosphere() { PLANET_RADIUS } else { 0.0 },
                ),
                near_frame,
                fine_frame,
                coarse_frame,
            },
            near: near_handle,
            fine: fine_handle,
            coarse: images.add(coarse_image),
        },
    });

    // lakes: the map (they're only where the ground is below the level); the sea: to the horizon
    let mesh = if w.sea {
        meshes.add(disc(SEA_RADIUS))
    } else {
        let half = coarse.spacing * (coarse.n - 1) as f32 * 0.5;
        meshes.add(Plane3d::new(Vec3::Y, Vec2::splat(half)).mesh())
    };
    let mut e = commands.spawn((
        Name::new(if w.sea { "Sea" } else { "Lakes" }),
        Mesh3d(mesh),
        MeshMaterial3d(material),
        Transform::from_xyz(0.0, w.level, 0.0),
        bevy::light::NotShadowCaster,
    ));
    if w.sea {
        e.insert(FollowCamera);
    }
    e.id()
}

/// A flat disc with rings that widen outwards (it follows the camera, so the
/// small triangles are always underneath: one huge triangle would lose precision
/// in the interpolated world position up close).
fn disc(radius: f32) -> Mesh {
    const SEGMENTS: usize = 32;
    let mut radii = vec![0.0f32];
    let mut r = 60.0;
    while r < radius {
        radii.push(r);
        r *= 1.4;
    }
    radii.push(radius);
    let mut positions = Vec::new();
    for (k, &r) in radii.iter().enumerate() {
        let n = if k == 0 { 1 } else { SEGMENTS };
        for s in 0..n {
            let a = s as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
            positions.push([r * a.cos(), 0.0, r * a.sin()]);
        }
    }
    let ring = |k: usize, s: usize| (1 + (k - 1) * SEGMENTS + s % SEGMENTS) as u32;
    let mut indices = Vec::new();
    for s in 0..SEGMENTS {
        // counter-clockwise from above
        indices.extend_from_slice(&[0, ring(1, s + 1), ring(1, s)]);
    }
    for k in 1..radii.len() - 1 {
        for s in 0..SEGMENTS {
            let (a, b, c, d) = (ring(k, s), ring(k, s + 1), ring(k + 1, s), ring(k + 1, s + 1));
            indices.extend_from_slice(&[a, b, c, b, d, c]);
        }
    }
    let count = positions.len();
    Mesh::new(bevy::mesh::PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, vec![[0.0f32, 1.0, 0.0]; count])
        .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, vec![[0.0f32, 0.0]; count])
        .with_inserted_indices(bevy::mesh::Indices::U32(indices))
}

pub(super) fn follow_camera(
    camera: Query<&GlobalTransform, With<crate::camera::MainCamera>>,
    mut water: Query<&mut Transform, With<FollowCamera>>,
) {
    let Ok(cam) = camera.single() else { return };
    let p = cam.translation();
    for mut tf in &mut water {
        tf.translation.x = p.x;
        tf.translation.z = p.z;
    }
}
