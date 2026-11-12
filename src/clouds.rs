//! Raymarched volumetric clouds.
//!
//! A cumulus / strato-cumulus layer between two spherical shells around the
//! planet, shaped by a weather map (coverage + cloud type), Perlin-Worley base
//! noise and Worley erosion noise, all baked on the CPU at startup. The GPU
//! side (`assets/shaders/clouds.wgsl`) marches it at reduced resolution right
//! after the opaque pass, then a depth-aware upsample composites it as the
//! first item of the transparent pass, so it sits behind plumes and trails but
//! in front of the sky and distant terrain.
//!
//! * `FLIGHT_SIM_NO_CLOUDS=1` disables the plugin.
//! * `CLOUDS_DIV=<n>` renders the clouds at 1/n resolution per axis.
//! * `CLOUDS_BLEND=<0..1>` weight of the new frame in the temporal
//!   accumulation (1 disables it).
//! * `CLOUDS_DEBUG=1` shows cloud opacity instead of colour.
//! * `CLOUDS_DEBUG_CAM=<above|inside|below|sun|horizon>` parks the camera at a
//!   cloud-viewing spot (for screenshots).
//! * `CLOUDS_NOVSYNC=1` disables vsync in windowed runs, for timing.
//!
//! The CPU-side [`CloudField`] mirrors the coverage map and height profile so
//! gameplay can ask how much cloud is at a point.

mod noise;

use std::sync::Arc;

use bevy::{
    core_pipeline::{
        Core3d,
        core_3d::{
            CORE_3D_DEPTH_FORMAT, Transparent3d, TransparentSortingInfo3d, main_opaque_pass_3d,
            main_transparent_pass_3d,
        },
    },
    ecs::{
        query::ROQueryItem,
        system::{SystemParamItem, lifetimeless::SRes},
    },
    prelude::*,
    render::{
        Extract, ExtractSchedule, Render, RenderApp, RenderStartup, RenderSystems,
        camera::ExtractedCamera,
        render_phase::{
            AddRenderCommand, DrawFunctions, PhaseItem, PhaseItemExtraIndex, RenderCommand,
            RenderCommandResult, SetItemPipeline, TrackedRenderPass, ViewSortedRenderPhases,
        },
        render_resource::{
            binding_types::{
                sampler, texture_2d, texture_3d, texture_depth_2d, texture_depth_2d_multisampled,
                uniform_buffer,
            },
            *,
        },
        renderer::{RenderContext, RenderDevice, RenderQueue, ViewQuery},
        sync_world::{MainEntity, RenderEntity},
        texture::{CachedTexture, TextureCache},
        view::{ExtractedView, Msaa, ViewDepthStencilTexture},
    },
};

use crate::camera::MainCamera;

pub struct CloudsPlugin;

/// Planet radius the layer (and Bevy's atmosphere) curves around.
const PLANET_RADIUS: f32 = 6_360_000.0;
const BASE_NOISE_SIZE: u32 = 128;
const DETAIL_NOISE_SIZE: u32 = 32;
const WEATHER_SIZE: u32 = 512;

impl Plugin for CloudsPlugin {
    fn build(&self, app: &mut App) {
        let disabled = std::env::var("FLIGHT_SIM_NO_CLOUDS").is_ok();
        let mut settings = CloudSettings::default();
        if let Some(div) = std::env::var("CLOUDS_DIV")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            settings.divisor = div;
        }
        if let Some(d) = std::env::var("CLOUDS_DEBUG")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            settings.debug = d;
        }
        if let Some(b) = std::env::var("CLOUDS_BLEND")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            settings.temporal_blend = b;
        }
        settings.enabled = !disabled;

        let t0 = std::time::Instant::now();
        let weather = Arc::new(noise::weather_map(WEATHER_SIZE, settings.coverage_bias));
        app.insert_resource(CloudField::new(weather.clone(), &settings))
            .add_systems(Update, update_field_time);
        if disabled {
            app.insert_resource(settings);
            info!("clouds disabled (FLIGHT_SIM_NO_CLOUDS)");
            return;
        }

        let base = noise::base_shape(BASE_NOISE_SIZE);
        let detail = noise::detail_shape(DETAIL_NOISE_SIZE);
        let weather_bytes: Vec<u8> = weather
            .iter()
            .flat_map(|w| [(w.coverage * 255.0) as u8, (w.cloud_type * 255.0) as u8])
            .collect();
        info!(
            "baked cloud noise ({}^3 base, {}^3 detail, {}^2 weather) in {:.0} ms",
            BASE_NOISE_SIZE,
            DETAIL_NOISE_SIZE,
            WEATHER_SIZE,
            t0.elapsed().as_secs_f32() * 1000.0
        );

        app.insert_resource(settings.clone())
            .add_systems(Update, mark_camera)
            .add_systems(PostUpdate, debug_camera.after(TransformSystems::Propagate));
        if std::env::var("CLOUDS_NOVSYNC").is_ok() {
            app.add_systems(Startup, disable_vsync);
        }

        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app
            .insert_resource(settings)
            .insert_resource(CloudNoiseData {
                base: Arc::new(base),
                detail: Arc::new(detail),
                weather: Arc::new(weather_bytes),
            })
            .init_resource::<SpecializedRenderPipelines<CloudsPipelines>>()
            .init_resource::<ExtractedSun>()
            .add_render_command::<Transparent3d, DrawClouds>()
            .add_systems(RenderStartup, init_clouds_pipelines)
            .add_systems(ExtractSchedule, extract_clouds)
            .add_systems(
                Render,
                (
                    configure_depth_usage.in_set(RenderSystems::PrepareViews),
                    queue_clouds.in_set(RenderSystems::Queue),
                    (prepare_clouds_textures, prepare_clouds_uniforms)
                        .chain()
                        .in_set(RenderSystems::PrepareResources),
                    prepare_clouds_bind_groups.in_set(RenderSystems::PrepareBindGroups),
                ),
            )
            .add_systems(
                Core3d,
                clouds_raymarch_pass
                    .after(main_opaque_pass_3d)
                    .before(main_transparent_pass_3d),
            );
    }
}

// ---------------------------------------------------------------------------
// settings and the CPU-side field

/// Everything that shapes and lights the clouds. Lives in both worlds.
#[derive(Resource, Clone)]
pub struct CloudSettings {
    pub enabled: bool,
    /// Altitude of the cloud base, metres.
    pub layer_bottom: f32,
    /// Altitude of the top of the layer, metres.
    pub layer_top: f32,
    /// Metres per repeat of the weather map.
    pub weather_scale: f32,
    /// Metres per repeat of the base shape noise.
    pub base_scale: f32,
    /// Metres per repeat of the erosion noise.
    pub detail_scale: f32,
    /// Added to the weather map's coverage; positive = cloudier.
    pub coverage_bias: f32,
    /// Extinction coefficient at full density, per metre.
    pub extinction: f32,
    /// Clouds beyond this distance aren't marched.
    pub max_distance: f32,
    /// Clouds nearer than this are not faded at all.
    pub fade_start: f32,
    /// e-folding distance of the fade into the sky beyond `fade_start`.
    pub fade_distance: f32,
    /// Fudge on the single-scattering sun term (real clouds bounce a lot more
    /// light around than three octaves capture).
    pub sun_mult: f32,
    pub ambient_mult: f32,
    /// Strength of the near-isotropic multiple-scattering term (roughly the
    /// cloud's diffuse reflectance).
    pub ms_albedo: f32,
    /// Erosion detail is faded out between these distances (metres), where
    /// the march steps are too coarse to resolve it without aliasing.
    pub detail_fade_start: f32,
    pub detail_fade_end: f32,
    /// Sky radiance seen from the cloud tops / bases, pre-exposure.
    pub ambient_top: Vec3,
    pub ambient_bottom: Vec3,
    /// Atmospheric tint on the sun by the time it reaches the layer.
    pub sun_tint: Vec3,
    pub wind: Vec2,
    /// Render at 1/divisor resolution per axis (3 = 1/9 of the pixels; the
    /// temporal accumulation and depth-aware upsample hide most of it).
    pub divisor: u32,
    pub max_steps: u32,
    /// Smallest march step, metres; steps grow with distance.
    pub step_min: f32,
    pub step_scale: f32,
    /// Weight of the new frame in the temporal accumulation (1 = off).
    pub temporal_blend: f32,
    pub debug: u32,
}

impl Default for CloudSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            layer_bottom: 1_900.0,
            layer_top: 4_300.0,
            weather_scale: 48_000.0,
            base_scale: 6_000.0,
            detail_scale: 700.0,
            coverage_bias: 0.0,
            extinction: 0.07,
            max_distance: 60_000.0,
            fade_start: 6_000.0,
            fade_distance: 22_000.0,
            sun_mult: 1.3,
            ambient_mult: 1.6,
            ms_albedo: 0.95,
            detail_fade_start: 10_000.0,
            detail_fade_end: 30_000.0,
            ambient_top: Vec3::new(0.7, 0.8, 1.0) * 6_500.0,
            ambient_bottom: Vec3::new(0.72, 0.76, 0.84) * 6_500.0,
            sun_tint: Vec3::new(1.0, 0.98, 0.94),
            wind: Vec2::new(6.0, 2.0),
            divisor: 3,
            max_steps: 128,
            step_min: 32.0,
            step_scale: 0.025,
            temporal_blend: 0.12,
            debug: 0,
        }
    }
}

/// CPU mirror of the cloud layer: the same weather map and height profile the
/// shader uses, minus the shape and detail noise. Good enough for "is this
/// point in cloud" queries from AI and seekers.
#[derive(Resource)]
#[allow(dead_code)] // gameplay hook, not wired into AI / seekers yet
pub struct CloudField {
    weather: Arc<Vec<noise::Weather>>,
    layer_bottom: f32,
    layer_top: f32,
    weather_scale: f32,
    coverage_bias: f32,
    wind: Vec2,
    enabled: bool,
    time: f32,
}

#[allow(dead_code)] // gameplay hook, not wired into AI / seekers yet
impl CloudField {
    fn new(weather: Arc<Vec<noise::Weather>>, s: &CloudSettings) -> Self {
        Self {
            weather,
            layer_bottom: s.layer_bottom,
            layer_top: s.layer_top,
            weather_scale: s.weather_scale,
            coverage_bias: s.coverage_bias,
            wind: s.wind,
            enabled: s.enabled,
            time: 0.0,
        }
    }

    /// Altitudes (metres) of the bottom and top of the cloud layer.
    pub fn layer(&self) -> (f32, f32) {
        (self.layer_bottom, self.layer_top)
    }

    /// Approximate cloud density at a world position, 0 (clear) to 1 (thick).
    pub fn density_at(&self, p: Vec3) -> f32 {
        if !self.enabled {
            return 0.0;
        }
        let altitude = (p - Vec3::new(0.0, -PLANET_RADIUS, 0.0)).length() - PLANET_RADIUS;
        let h = (altitude - self.layer_bottom) / (self.layer_top - self.layer_bottom);
        if !(0.0..=1.0).contains(&h) {
            return 0.0;
        }
        let w = self.weather_at(p.x, p.z);
        let coverage = (w.coverage + self.coverage_bias).clamp(0.0, 1.0);
        let grad = height_gradient(h, w.cloud_type);
        if coverage <= 0.0 || grad <= 0.0 {
            return 0.0;
        }
        // the shape noise averages out to about 0.55 in both channels
        let base = noise::remap(0.55, 0.55 - 1.0, 1.0, 0.0, 1.0) * grad;
        let cov = coverage * (1.0 - 0.2 * h);
        (noise::remap(base, 1.0 - cov, 1.0, 0.0, 1.0) * cov).clamp(0.0, 1.0)
    }

    /// Fraction of sky covered by cloud above a ground position.
    pub fn coverage_at(&self, x: f32, z: f32) -> f32 {
        if !self.enabled {
            return 0.0;
        }
        (self.weather_at(x, z).coverage + self.coverage_bias).clamp(0.0, 1.0)
    }

    fn weather_at(&self, x: f32, z: f32) -> noise::Weather {
        let n = WEATHER_SIZE as f32;
        let uv = (Vec2::new(x, z) + self.wind * self.time) / self.weather_scale;
        let s = (uv - uv.floor()) * n - 0.5;
        let fl = s.floor();
        let fr = s - fl;
        let at = |dx: i32, dz: i32| {
            let ix = (fl.x as i32 + dx).rem_euclid(WEATHER_SIZE as i32) as usize;
            let iz = (fl.y as i32 + dz).rem_euclid(WEATHER_SIZE as i32) as usize;
            self.weather[iz * WEATHER_SIZE as usize + ix]
        };
        let (a, b, c, d) = (at(0, 0), at(1, 0), at(0, 1), at(1, 1));
        let lerp = |p: f32, q: f32, t: f32| p + (q - p) * t;
        let bilerp = |f: fn(&noise::Weather) -> f32| {
            lerp(lerp(f(&a), f(&b), fr.x), lerp(f(&c), f(&d), fr.x), fr.y)
        };
        noise::Weather {
            coverage: bilerp(|w| w.coverage),
            cloud_type: bilerp(|w| w.cloud_type),
        }
    }
}

/// Same curve as `height_gradient` in the shader.
#[allow(dead_code)] // used by CloudField
fn height_gradient(h: f32, cloud_type: f32) -> f32 {
    let top = 0.28 + (1.0 - 0.28) * cloud_type;
    noise::smoothstep(0.0, 0.08, h) * (1.0 - noise::smoothstep(top * 0.65, top, h))
}

fn update_field_time(time: Res<Time>, mut field: ResMut<CloudField>) {
    field.time = time.elapsed_secs();
}

/// Main-world marker for cameras that get clouds.
#[derive(Component)]
struct CloudsCamera;

#[allow(clippy::type_complexity)]
fn mark_camera(
    mut commands: Commands,
    mut cameras: Query<(Entity, &mut Camera3d), (Added<MainCamera>, Without<CloudsCamera>)>,
) {
    for (entity, mut camera) in &mut cameras {
        // we read the main depth buffer, so it needs to be bindable
        let mut usages = TextureUsages::from(camera.depth_texture_usages);
        usages |= TextureUsages::TEXTURE_BINDING;
        camera.depth_texture_usages = usages.into();
        commands.entity(entity).insert(CloudsCamera);
    }
}

fn disable_vsync(mut windows: Query<&mut Window, With<bevy::window::PrimaryWindow>>) {
    for mut window in &mut windows {
        window.present_mode = bevy::window::PresentMode::AutoNoVsync;
    }
}

/// `CLOUDS_DEBUG_CAM=<preset>`: override the camera for cloud screenshots.
fn debug_camera(
    mut camera: Query<(&mut Transform, &mut GlobalTransform), With<MainCamera>>,
    settings: Res<CloudSettings>,
    field: Res<CloudField>,
    terrain: Res<crate::world::Terrain>,
    mut pose: Local<Option<Option<(Vec3, Vec3)>>>,
) {
    let pose = pose.get_or_insert_with(|| {
        let preset = std::env::var("CLOUDS_DEBUG_CAM").ok()?;
        Some(debug_pose(&preset, &settings, &field, &terrain))
    });
    let Some((pos, dir)) = *pose else {
        return;
    };
    for (mut transform, mut global) in &mut camera {
        *transform = Transform::from_translation(pos).looking_to(dir, Vec3::Y);
        *global = GlobalTransform::from(*transform);
    }
}

/// Camera position and look direction for a `CLOUDS_DEBUG_CAM` preset.
fn debug_pose(
    preset: &str,
    settings: &CloudSettings,
    field: &CloudField,
    terrain: &crate::world::Terrain,
) -> (Vec3, Vec3) {
    let (bottom, top) = (settings.layer_bottom, settings.layer_top);
    let sun = Vec3::new(4.0, 10.0, 10.0).normalize();
    // for the sun shot, find a spot with thick cloud between the camera and the sun
    let sun_spot = || {
        let mut best = (0.0, Vec3::new(0.0, 1_000.0, 0.0));
        for ix in -25..=25 {
            for iz in -25..=25 {
                let (x, z) = (ix as f32 * 1_000.0, iz as f32 * 1_000.0);
                // low ground only, so the camera sits well clear of the mountains
                let ground = terrain.height_at(x, z);
                if ground > 600.0 {
                    continue;
                }
                let eye = ground + 500.0;
                let t = ((bottom + top) * 0.5 - eye) / sun.y;
                let cam = Vec3::new(x, eye, z);
                let p = cam + sun * t;
                let score = field.coverage_at(p.x, p.z) - 0.5 * field.coverage_at(x, z);
                if score > best.0 {
                    best = (score, cam);
                }
            }
        }
        best.1
    };
    match preset {
        "above" => (
            Vec3::new(0.0, top + 1_200.0, 0.0),
            Vec3::new(0.3, -0.28, 0.9).normalize(),
        ),
        "inside" => {
            // in the layer at the edge of a bank, looking into its wall
            let mut best = (0.0, Vec3::new(0.0, bottom + 500.0, 0.0));
            for ix in -25..=25 {
                for iz in -25..=25 {
                    let (x, z) = (ix as f32 * 1_000.0, iz as f32 * 1_000.0);
                    let here = field.coverage_at(x, z);
                    let ahead = field.coverage_at(x, z - 2_500.0);
                    let score = ahead - here - (here - 0.35).abs();
                    if score > best.0 {
                        best = (score, Vec3::new(x, bottom + 500.0, z));
                    }
                }
            }
            (best.1, Vec3::new(0.0, 0.08, -1.0).normalize())
        }
        "below" => (
            Vec3::new(0.0, 900.0, 0.0),
            Vec3::new(0.0, 0.55, -1.0).normalize(),
        ),
        "sun" => (sun_spot(), (sun - Vec3::Y * 0.25).normalize()),
        _ => (Vec3::new(0.0, 1_500.0, 6_000.0), -Vec3::Z),
    }
}

// ---------------------------------------------------------------------------
// render world: extraction

/// Noise texture bytes, handed to the render world once.
#[derive(Resource)]
struct CloudNoiseData {
    base: Arc<Vec<u8>>,
    detail: Arc<Vec<u8>>,
    weather: Arc<Vec<u8>>,
}

#[derive(Resource, Default)]
struct ExtractedSun {
    dir_to_light: Vec3,
    /// Linear colour times illuminance (lux).
    color: Vec3,
    time: f32,
}

/// Render-world marker for views that get clouds.
#[derive(Component)]
struct CloudsView;

fn extract_clouds(
    mut commands: Commands,
    mut sun: ResMut<ExtractedSun>,
    mut settings: ResMut<CloudSettings>,
    main_settings: Extract<Res<CloudSettings>>,
    time: Extract<Res<Time>>,
    lights: Extract<Query<(&GlobalTransform, &DirectionalLight)>>,
    cameras: Extract<Query<&RenderEntity, With<CloudsCamera>>>,
) {
    if main_settings.is_changed() {
        *settings = main_settings.clone();
    }
    if let Some((transform, light)) = lights
        .iter()
        .max_by(|a, b| a.1.illuminance.total_cmp(&b.1.illuminance))
    {
        sun.dir_to_light = transform.back().as_vec3();
        let c = light.color.to_linear();
        sun.color = Vec3::new(c.red, c.green, c.blue) * light.illuminance;
    }
    sun.time = time.elapsed_secs();
    for render_entity in &cameras {
        commands.entity(render_entity.id()).insert(CloudsView);
    }
}

fn configure_depth_usage(mut cameras: Query<&mut Camera3d, With<CloudsView>>) {
    for mut camera in &mut cameras {
        let mut usages = TextureUsages::from(camera.depth_texture_usages);
        if !usages.contains(TextureUsages::TEXTURE_BINDING) {
            usages |= TextureUsages::TEXTURE_BINDING;
            camera.depth_texture_usages = usages.into();
        }
    }
}

// ---------------------------------------------------------------------------
// render world: pipelines and GPU resources

#[derive(Clone, Copy, ShaderType)]
struct CloudsUniform {
    world_from_clip: Mat4,
    prev_clip_from_world: Mat4,
    camera_pos: Vec3,
    time: f32,
    sun_dir: Vec3,
    exposure: f32,
    sun_color: Vec3,
    planet_radius: f32,
    ambient_top: Vec3,
    layer_bottom: f32,
    ambient_bottom: Vec3,
    layer_top: f32,
    full_size: Vec2,
    low_size: Vec2,
    wind: Vec2,
    weather_scale: f32,
    base_scale: f32,
    detail_scale: f32,
    coverage_bias: f32,
    extinction: f32,
    max_distance: f32,
    fade_distance: f32,
    sun_mult: f32,
    ambient_mult: f32,
    ms_albedo: f32,
    detail_fade_start: f32,
    detail_fade_end: f32,
    divisor: u32,
    max_steps: u32,
    step_min: f32,
    step_scale: f32,
    debug: u32,
    frame: u32,
    history_valid: u32,
    temporal_blend: f32,
    fade_start: f32,
}

/// The shader is embedded so the two depth-texture variants can be stamped
/// out at startup (Bevy 0.20 has no preprocessor for plain WGSL assets).
const SHADER_SOURCE: &str = include_str!("../assets/shaders/clouds.wgsl");

#[derive(Resource)]
struct CloudsPipelines {
    /// [single-sampled depth, multisampled depth]
    shader: [Handle<Shader>; 2],
    /// [single-sampled depth, multisampled depth]
    linearize_layout: [BindGroupLayoutDescriptor; 2],
    raymarch_layout: BindGroupLayoutDescriptor,
    resolve_layout: BindGroupLayoutDescriptor,
    composite_layout: BindGroupLayoutDescriptor,
    repeat_sampler: Sampler,
    clamp_sampler: Sampler,
    base: TextureView,
    detail: TextureView,
    weather: TextureView,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum CloudsPipelineKey {
    Linearize {
        msaa_samples: u32,
    },
    Raymarch,
    Resolve,
    Composite {
        msaa_samples: u32,
        target_format: TextureFormat,
    },
}

fn init_clouds_pipelines(
    mut commands: Commands,
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    asset_server: Res<AssetServer>,
    noise: Res<CloudNoiseData>,
) {
    let linearize_layout = |multisampled: bool, label: &'static str| {
        let depth = if multisampled {
            texture_depth_2d_multisampled()
        } else {
            texture_depth_2d()
        };
        BindGroupLayoutDescriptor::new(
            label,
            &BindGroupLayoutEntries::with_indices(
                ShaderStages::FRAGMENT,
                ((0, uniform_buffer::<CloudsUniform>(false)), (5, depth)),
            ),
        )
    };
    let raymarch_layout = BindGroupLayoutDescriptor::new(
        "clouds_raymarch_layout",
        &BindGroupLayoutEntries::with_indices(
            ShaderStages::FRAGMENT,
            (
                (0, uniform_buffer::<CloudsUniform>(false)),
                (1, texture_3d(TextureSampleType::Float { filterable: true })),
                (2, texture_3d(TextureSampleType::Float { filterable: true })),
                (3, texture_2d(TextureSampleType::Float { filterable: true })),
                (4, sampler(SamplerBindingType::Filtering)),
                (
                    6,
                    texture_2d(TextureSampleType::Float { filterable: false }),
                ),
            ),
        ),
    );
    let resolve_layout = BindGroupLayoutDescriptor::new(
        "clouds_resolve_layout",
        &BindGroupLayoutEntries::with_indices(
            ShaderStages::FRAGMENT,
            (
                (0, uniform_buffer::<CloudsUniform>(false)),
                (
                    1,
                    texture_2d(TextureSampleType::Float { filterable: false }),
                ),
                (
                    2,
                    texture_2d(TextureSampleType::Float { filterable: false }),
                ),
                (7, texture_2d(TextureSampleType::Float { filterable: true })),
                (8, texture_2d(TextureSampleType::Float { filterable: true })),
                (9, sampler(SamplerBindingType::Filtering)),
            ),
        ),
    );
    let composite_layout = BindGroupLayoutDescriptor::new(
        "clouds_composite_layout",
        &BindGroupLayoutEntries::with_indices(
            ShaderStages::FRAGMENT,
            (
                (0, uniform_buffer::<CloudsUniform>(false)),
                (
                    1,
                    texture_2d(TextureSampleType::Float { filterable: false }),
                ),
                (
                    2,
                    texture_2d(TextureSampleType::Float { filterable: false }),
                ),
                (
                    6,
                    texture_2d(TextureSampleType::Float { filterable: false }),
                ),
            ),
        ),
    );

    let tex3d = |data: &[u8], size: u32, label: &'static str| {
        device
            .create_texture_with_data(
                &queue,
                &TextureDescriptor {
                    label: Some(label),
                    size: Extent3d {
                        width: size,
                        height: size,
                        depth_or_array_layers: size,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: TextureDimension::D3,
                    format: TextureFormat::Rgba8Unorm,
                    usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
                    view_formats: &[],
                },
                TextureDataOrder::default(),
                data,
            )
            .create_view(&TextureViewDescriptor::default())
    };
    let base = tex3d(&noise.base, BASE_NOISE_SIZE, "clouds_base_noise");
    let detail = tex3d(&noise.detail, DETAIL_NOISE_SIZE, "clouds_detail_noise");
    let weather = device
        .create_texture_with_data(
            &queue,
            &TextureDescriptor {
                label: Some("clouds_weather"),
                size: Extent3d {
                    width: WEATHER_SIZE,
                    height: WEATHER_SIZE,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format: TextureFormat::Rg8Unorm,
                usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
                view_formats: &[],
            },
            TextureDataOrder::default(),
            &noise.weather,
        )
        .create_view(&TextureViewDescriptor::default());

    commands.insert_resource(CloudsPipelines {
        shader: [false, true].map(|ms| {
            let depth_type = if ms {
                "texture_depth_multisampled_2d"
            } else {
                "texture_depth_2d"
            };
            asset_server.add(Shader::from_wgsl(
                SHADER_SOURCE.replace("DEPTH_TEXTURE_TYPE", depth_type),
                format!("clouds.wgsl (multisampled: {ms})"),
            ))
        }),
        linearize_layout: [
            linearize_layout(false, "clouds_linearize_layout"),
            linearize_layout(true, "clouds_linearize_layout_ms"),
        ],
        raymarch_layout,
        resolve_layout,
        composite_layout,
        clamp_sampler: device.create_sampler(&SamplerDescriptor {
            label: Some("clouds_clamp_sampler"),
            address_mode_u: AddressMode::ClampToEdge,
            address_mode_v: AddressMode::ClampToEdge,
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            ..default()
        }),
        repeat_sampler: device.create_sampler(&SamplerDescriptor {
            label: Some("clouds_repeat_sampler"),
            address_mode_u: AddressMode::Repeat,
            address_mode_v: AddressMode::Repeat,
            address_mode_w: AddressMode::Repeat,
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            mipmap_filter: MipmapFilterMode::Linear,
            ..default()
        }),
        base,
        detail,
        weather,
    });
    commands.remove_resource::<CloudNoiseData>();
}

impl SpecializedRenderPipeline for CloudsPipelines {
    type Key = CloudsPipelineKey;

    fn specialize(&self, key: Self::Key) -> RenderPipelineDescriptor {
        let msaa_samples = match key {
            CloudsPipelineKey::Linearize { msaa_samples }
            | CloudsPipelineKey::Composite { msaa_samples, .. } => msaa_samples,
            CloudsPipelineKey::Raymarch | CloudsPipelineKey::Resolve => 1,
        };
        let low_res_targets = vec![
            Some(ColorTargetState {
                format: TextureFormat::Rgba16Float,
                blend: None,
                write_mask: ColorWrites::ALL,
            }),
            Some(ColorTargetState {
                format: TextureFormat::Rg16Float,
                blend: None,
                write_mask: ColorWrites::ALL,
            }),
        ];
        let ms = usize::from(msaa_samples > 1);
        let shader = self.shader[ms].clone();
        let vertex = VertexState {
            shader: shader.clone(),
            entry_point: Some("vs_fullscreen".into()),
            ..default()
        };
        match key {
            CloudsPipelineKey::Linearize { .. } => RenderPipelineDescriptor {
                label: Some("clouds_linearize_pipeline".into()),
                layout: vec![self.linearize_layout[ms].clone()],
                vertex,
                fragment: Some(FragmentState {
                    shader,
                    entry_point: Some("fs_linearize".into()),
                    targets: vec![Some(ColorTargetState {
                        format: TextureFormat::R32Float,
                        blend: None,
                        write_mask: ColorWrites::ALL,
                    })],
                    ..default()
                }),
                ..default()
            },
            CloudsPipelineKey::Raymarch => RenderPipelineDescriptor {
                label: Some("clouds_raymarch_pipeline".into()),
                layout: vec![self.raymarch_layout.clone()],
                vertex,
                fragment: Some(FragmentState {
                    shader: shader.clone(),
                    entry_point: Some("fs_raymarch".into()),
                    targets: low_res_targets,
                    ..default()
                }),
                ..default()
            },
            CloudsPipelineKey::Resolve => RenderPipelineDescriptor {
                label: Some("clouds_resolve_pipeline".into()),
                layout: vec![self.resolve_layout.clone()],
                vertex,
                fragment: Some(FragmentState {
                    shader: shader.clone(),
                    entry_point: Some("fs_resolve".into()),
                    targets: low_res_targets,
                    ..default()
                }),
                ..default()
            },
            CloudsPipelineKey::Composite { target_format, .. } => RenderPipelineDescriptor {
                label: Some("clouds_composite_pipeline".into()),
                layout: vec![self.composite_layout.clone()],
                vertex,
                fragment: Some(FragmentState {
                    shader,
                    entry_point: Some("fs_composite".into()),
                    targets: vec![Some(ColorTargetState {
                        format: target_format,
                        // premultiplied alpha
                        blend: Some(BlendState {
                            color: BlendComponent {
                                src_factor: BlendFactor::One,
                                dst_factor: BlendFactor::OneMinusSrcAlpha,
                                operation: BlendOperation::Add,
                            },
                            alpha: BlendComponent {
                                src_factor: BlendFactor::One,
                                dst_factor: BlendFactor::OneMinusSrcAlpha,
                                operation: BlendOperation::Add,
                            },
                        }),
                        write_mask: ColorWrites::ALL,
                    })],
                    ..default()
                }),
                depth_stencil: Some(DepthStencilState {
                    format: CORE_3D_DEPTH_FORMAT,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(CompareFunction::Always),
                    stencil: StencilState::default(),
                    bias: DepthBiasState::default(),
                }),
                multisample: MultisampleState {
                    count: msaa_samples,
                    ..default()
                },
                ..default()
            },
        }
    }
}

#[derive(Component)]
struct CloudsViewPipelines {
    linearize: CachedRenderPipelineId,
    raymarch: CachedRenderPipelineId,
    resolve: CachedRenderPipelineId,
}

#[derive(Component)]
struct CloudsViewTextures {
    /// full-res distance to the scene, metres
    dist: CachedTexture,
    /// this frame's raw march
    color: CachedTexture,
    data: CachedTexture,
    /// temporally resolved output, ping-ponged between frames
    resolved_color: [CachedTexture; 2],
    resolved_data: [CachedTexture; 2],
    full_size: UVec2,
    low_size: UVec2,
}

/// Per-view temporal state, carried across frames.
#[derive(Component, Default)]
struct CloudsViewHistory {
    frame: u32,
    valid: bool,
    low_size: UVec2,
    prev_clip_from_world: Mat4,
}

impl CloudsViewHistory {
    /// Index of the resolved texture written this frame.
    fn write_index(&self) -> usize {
        (self.frame % 2) as usize
    }
}

#[derive(Component)]
struct CloudsViewUniform(UniformBuffer<CloudsUniform>);

#[derive(Component)]
struct CloudsViewBindGroups {
    linearize: BindGroup,
    raymarch: BindGroup,
    resolve: BindGroup,
    composite: BindGroup,
}

/// Specialises both pipelines for the view and queues the composite draw as
/// the farthest transparent item.
#[allow(clippy::too_many_arguments)]
fn queue_clouds(
    mut commands: Commands,
    draw_functions: Res<DrawFunctions<Transparent3d>>,
    pipeline_cache: Res<PipelineCache>,
    pipelines: Option<Res<CloudsPipelines>>,
    mut specialized: ResMut<SpecializedRenderPipelines<CloudsPipelines>>,
    mut phases: ResMut<ViewSortedRenderPhases<Transparent3d>>,
    settings: Res<CloudSettings>,
    views: Query<(Entity, &ExtractedView, &Msaa), With<CloudsView>>,
) {
    let Some(pipelines) = pipelines else {
        return;
    };
    if !settings.enabled {
        return;
    }
    let draw_function = draw_functions.read().id::<DrawClouds>();
    for (entity, view, msaa) in &views {
        let Some(phase) = phases.get_mut(&view.retained_view_entity) else {
            continue;
        };
        let linearize = specialized.specialize(
            &pipeline_cache,
            &pipelines,
            CloudsPipelineKey::Linearize {
                msaa_samples: msaa.samples(),
            },
        );
        let raymarch =
            specialized.specialize(&pipeline_cache, &pipelines, CloudsPipelineKey::Raymarch);
        let resolve =
            specialized.specialize(&pipeline_cache, &pipelines, CloudsPipelineKey::Resolve);
        let composite = specialized.specialize(
            &pipeline_cache,
            &pipelines,
            CloudsPipelineKey::Composite {
                msaa_samples: msaa.samples(),
                target_format: view.target_format,
            },
        );
        commands.entity(entity).insert(CloudsViewPipelines {
            linearize,
            raymarch,
            resolve,
        });
        phase.add_transient(Transparent3d {
            sorting_info: TransparentSortingInfo3d::Sorted {
                mesh_center: Vec3::ZERO,
                depth_bias: -1.0e30,
            },
            distance: -1.0e30,
            pipeline: composite,
            entity: (entity, MainEntity::from(Entity::PLACEHOLDER)),
            draw_function,
            batch_range: 0..1,
            extra_index: PhaseItemExtraIndex::None,
            indexed: false,
        });
    }
}

fn prepare_clouds_textures(
    mut commands: Commands,
    device: Res<RenderDevice>,
    mut texture_cache: ResMut<TextureCache>,
    settings: Res<CloudSettings>,
    mut views: Query<(Entity, &ExtractedCamera, Option<&mut CloudsViewHistory>), With<CloudsView>>,
) {
    if !settings.enabled {
        return;
    }
    for (entity, camera, history) in &mut views {
        let Some(full_size) = camera.physical_viewport_size else {
            continue;
        };
        let divisor = settings.divisor.max(1);
        let low_size = (full_size + divisor - 1) / divisor;
        let desc = |label: &'static str, format: TextureFormat, size: UVec2| TextureDescriptor {
            label: Some(label),
            size: Extent3d {
                width: size.x,
                height: size.y,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format,
            usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        let dist = texture_cache.get(
            &device,
            desc("clouds_dist", TextureFormat::R32Float, full_size),
        );
        let color = texture_cache.get(
            &device,
            desc("clouds_color", TextureFormat::Rgba16Float, low_size),
        );
        let data = texture_cache.get(
            &device,
            desc("clouds_data", TextureFormat::Rg16Float, low_size),
        );
        let resolved_color = [
            texture_cache.get(
                &device,
                desc(
                    "clouds_resolved_color_a",
                    TextureFormat::Rgba16Float,
                    low_size,
                ),
            ),
            texture_cache.get(
                &device,
                desc(
                    "clouds_resolved_color_b",
                    TextureFormat::Rgba16Float,
                    low_size,
                ),
            ),
        ];
        let resolved_data = [
            texture_cache.get(
                &device,
                desc("clouds_resolved_data_a", TextureFormat::Rg16Float, low_size),
            ),
            texture_cache.get(
                &device,
                desc("clouds_resolved_data_b", TextureFormat::Rg16Float, low_size),
            ),
        ];
        commands.entity(entity).insert(CloudsViewTextures {
            dist,
            color,
            data,
            resolved_color,
            resolved_data,
            full_size,
            low_size,
        });
        match history {
            Some(mut history) => {
                history.frame = history.frame.wrapping_add(1);
                history.valid = history.low_size == low_size && settings.temporal_blend < 1.0;
                history.low_size = low_size;
            }
            None => {
                commands.entity(entity).insert(CloudsViewHistory {
                    low_size,
                    ..default()
                });
            }
        }
    }
}

#[allow(clippy::type_complexity)]
fn prepare_clouds_uniforms(
    mut commands: Commands,
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    settings: Res<CloudSettings>,
    sun: Res<ExtractedSun>,
    mut views: Query<
        (
            Entity,
            &ExtractedView,
            &ExtractedCamera,
            &CloudsViewTextures,
            &mut CloudsViewHistory,
            Option<&mut CloudsViewUniform>,
        ),
        With<CloudsView>,
    >,
) {
    for (entity, view, camera, textures, mut history, existing) in &mut views {
        let world_from_view = view.world_from_view.to_matrix();
        let clip_from_world = view
            .clip_from_world
            .unwrap_or_else(|| view.clip_from_view * world_from_view.inverse());
        let prev_clip_from_world = if history.valid {
            history.prev_clip_from_world
        } else {
            clip_from_world
        };
        history.prev_clip_from_world = clip_from_world;
        let s = &*settings;
        let value = CloudsUniform {
            world_from_clip: clip_from_world.inverse(),
            prev_clip_from_world,
            camera_pos: view.world_from_view.translation(),
            time: sun.time,
            sun_dir: sun.dir_to_light,
            exposure: camera.exposure,
            sun_color: sun.color * s.sun_tint,
            planet_radius: PLANET_RADIUS,
            ambient_top: s.ambient_top,
            layer_bottom: s.layer_bottom,
            ambient_bottom: s.ambient_bottom,
            layer_top: s.layer_top,
            full_size: textures.full_size.as_vec2(),
            low_size: textures.low_size.as_vec2(),
            wind: s.wind,
            weather_scale: s.weather_scale,
            base_scale: s.base_scale,
            detail_scale: s.detail_scale,
            coverage_bias: s.coverage_bias,
            extinction: s.extinction,
            max_distance: s.max_distance,
            fade_distance: s.fade_distance,
            sun_mult: s.sun_mult,
            ambient_mult: s.ambient_mult,
            ms_albedo: s.ms_albedo,
            detail_fade_start: s.detail_fade_start,
            detail_fade_end: s.detail_fade_end,
            divisor: s.divisor.max(1),
            max_steps: s.max_steps,
            step_min: s.step_min,
            step_scale: s.step_scale,
            debug: s.debug,
            frame: history.frame,
            history_valid: u32::from(history.valid),
            temporal_blend: s.temporal_blend,
            fade_start: s.fade_start,
        };
        match existing {
            Some(mut uniform) => {
                uniform.0.set(value);
                uniform.0.write_buffer(&device, &queue);
            }
            None => {
                let mut buffer = UniformBuffer::from(value);
                buffer.set_label(Some("clouds_uniform"));
                buffer.write_buffer(&device, &queue);
                commands.entity(entity).insert(CloudsViewUniform(buffer));
            }
        }
    }
}

#[allow(clippy::type_complexity)]
fn prepare_clouds_bind_groups(
    mut commands: Commands,
    device: Res<RenderDevice>,
    pipeline_cache: Res<PipelineCache>,
    pipelines: Option<Res<CloudsPipelines>>,
    views: Query<
        (
            Entity,
            &Msaa,
            &CloudsViewTextures,
            &CloudsViewHistory,
            &CloudsViewUniform,
            &ViewDepthStencilTexture,
        ),
        With<CloudsView>,
    >,
) {
    let Some(pipelines) = pipelines else {
        return;
    };
    for (entity, msaa, textures, history, uniform, depth) in &views {
        let write = history.write_index();
        let read = 1 - write;
        let Some(uniform) = uniform.0.binding() else {
            continue;
        };
        let ms = usize::from(msaa.samples() > 1);
        let depth_view = &depth.attachment.texture.default_view;
        let linearize = device.create_bind_group(
            "clouds_linearize_bind_group",
            &pipeline_cache.get_bind_group_layout(&pipelines.linearize_layout[ms]),
            &BindGroupEntries::with_indices(((0, uniform.clone()), (5, depth_view))),
        );
        let raymarch = device.create_bind_group(
            "clouds_raymarch_bind_group",
            &pipeline_cache.get_bind_group_layout(&pipelines.raymarch_layout),
            &BindGroupEntries::with_indices((
                (0, uniform.clone()),
                (1, &pipelines.base),
                (2, &pipelines.detail),
                (3, &pipelines.weather),
                (4, &pipelines.repeat_sampler),
                (6, &textures.dist.default_view),
            )),
        );
        let resolve = device.create_bind_group(
            "clouds_resolve_bind_group",
            &pipeline_cache.get_bind_group_layout(&pipelines.resolve_layout),
            &BindGroupEntries::with_indices((
                (0, uniform.clone()),
                (1, &textures.color.default_view),
                (2, &textures.data.default_view),
                (7, &textures.resolved_color[read].default_view),
                (8, &textures.resolved_data[read].default_view),
                (9, &pipelines.clamp_sampler),
            )),
        );
        let composite = device.create_bind_group(
            "clouds_composite_bind_group",
            &pipeline_cache.get_bind_group_layout(&pipelines.composite_layout),
            &BindGroupEntries::with_indices((
                (0, uniform),
                (1, &textures.resolved_color[write].default_view),
                (2, &textures.resolved_data[write].default_view),
                (6, &textures.dist.default_view),
            )),
        );
        commands.entity(entity).insert(CloudsViewBindGroups {
            linearize,
            raymarch,
            resolve,
            composite,
        });
    }
}

// ---------------------------------------------------------------------------
// render world: the passes

/// Marches the clouds into the reduced-resolution targets. Runs alongside the
/// atmosphere's sky pass, after the opaque geometry has written depth.
fn clouds_raymarch_pass(
    view: ViewQuery<
        (
            &CloudsViewTextures,
            &CloudsViewHistory,
            &CloudsViewBindGroups,
            &CloudsViewPipelines,
        ),
        With<CloudsView>,
    >,
    pipeline_cache: Res<PipelineCache>,
    mut ctx: RenderContext,
) {
    let (textures, history, bind_groups, pipelines) = view.into_inner();
    let (Some(linearize), Some(raymarch), Some(resolve)) = (
        pipeline_cache.get_render_pipeline(pipelines.linearize),
        pipeline_cache.get_render_pipeline(pipelines.raymarch),
        pipeline_cache.get_render_pipeline(pipelines.resolve),
    ) else {
        return;
    };
    fn attachment(view: &TextureView) -> Option<RenderPassColorAttachment<'_>> {
        Some(RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: Operations {
                load: LoadOp::Clear(LinearRgba::NONE.into()),
                store: StoreOp::Store,
            },
        })
    }
    let encoder = ctx.command_encoder();
    {
        let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
            label: Some("clouds_linearize_depth"),
            color_attachments: &[attachment(&textures.dist.default_view)],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(linearize);
        pass.set_bind_group(0, &bind_groups.linearize, &[]);
        pass.draw(0..3, 0..1);
    }
    {
        let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
            label: Some("clouds_raymarch"),
            color_attachments: &[
                attachment(&textures.color.default_view),
                attachment(&textures.data.default_view),
            ],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(raymarch);
        pass.set_bind_group(0, &bind_groups.raymarch, &[]);
        pass.draw(0..3, 0..1);
    }
    let write = history.write_index();
    let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
        label: Some("clouds_temporal_resolve"),
        color_attachments: &[
            attachment(&textures.resolved_color[write].default_view),
            attachment(&textures.resolved_data[write].default_view),
        ],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    pass.set_pipeline(resolve);
    pass.set_bind_group(0, &bind_groups.resolve, &[]);
    pass.draw(0..3, 0..1);
}

type DrawClouds = (SetItemPipeline, DrawCloudsComposite);

/// Upsamples and blends the clouds over the scene inside the transparent pass.
struct DrawCloudsComposite;

impl<P: PhaseItem> RenderCommand<P> for DrawCloudsComposite {
    type Param = SRes<PipelineCache>;
    type ViewQuery = &'static CloudsViewBindGroups;
    type ItemQuery = ();

    fn render<'w>(
        _item: &P,
        bind_groups: ROQueryItem<'w, '_, Self::ViewQuery>,
        _entity: Option<()>,
        _param: SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        pass.set_bind_group(0, &bind_groups.composite, &[]);
        pass.draw(0..3, 0..1);
        RenderCommandResult::Success
    }
}
