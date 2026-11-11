//! CPU-simulated particles drawn in one sorted, premultiplied-alpha draw call.
//!
//! Every frame all live particles (plus the trail ribbons, which are fed in as
//! quads) are expanded into camera-facing quads, sorted back to front and
//! written into a single dynamic mesh. One material, one draw: explosions no
//! longer create a material per particle.
//!
//! The shader (`assets/shaders/fx_particles.wesl`) does the look: puffs are
//! lit by the sun using the normals baked into the puff atlas, fire is a
//! temperature-driven emissive term on the same puff (so a fireball cools
//! naturally into black smoke), and soft particles fade against the depth
//! prepass.

use bevy::{
    asset::RenderAssetUsages,
    camera::visibility::NoFrustumCulling,
    mesh::{Indices, MeshVertexAttribute, MeshVertexBufferLayoutRef, PrimitiveTopology},
    pbr::{MaterialPipeline, MaterialPipelineKey},
    prelude::*,
    render::render_resource::{
        AsBindGroup, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError,
        VertexFormat,
    },
    shader::ShaderRef,
};

use super::textures;
use crate::camera::MainCamera;

pub const MAX_PARTICLES: usize = 24_000;

/// Steady breeze that carries smoke (m/s).
pub const WIND: Vec3 = Vec3::new(5.0, 0.0, 2.0);

const ATTRIBUTE_FX_A: MeshVertexAttribute =
    MeshVertexAttribute::new("Fx_A", 917_352_001, VertexFormat::Float32x4);
const ATTRIBUTE_FX_B: MeshVertexAttribute =
    MeshVertexAttribute::new("Fx_B", 917_352_002, VertexFormat::Float32x4);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Billowy puff: lit smoke, dust, vapour, or (with heat) fire.
    Puff = 0,
    /// Velocity-stretched hot streak (sparks, fragments).
    Spark = 1,
    /// Soft additive glow (flashes, embers, muzzle flashes).
    Glow = 2,
    /// Trail ribbon segment shaded as a lit tube.
    Tube = 3,
}

#[derive(Clone, Copy, Debug)]
pub struct Particle {
    pub pos: Vec3,
    pub vel: Vec3,
    /// Negative ages delay the particle's appearance.
    pub age: f32,
    pub life: f32,
    /// Diameter at birth and at death.
    pub size: (f32, f32),
    /// Growth curve: size follows `1 - (1 - t)^grow`; big values pop out fast.
    pub grow: f32,
    pub rot: f32,
    pub spin: f32,
    /// How quickly the particle's velocity relaxes to the (windy) air, 1/s.
    pub drag: f32,
    /// How much the wind carries it, 0..1.
    pub wind: f32,
    pub gravity: f32,
    /// Upward acceleration, scaled by how hot the particle still is.
    pub buoyancy: f32,
    pub albedo: Vec3,
    pub opacity: f32,
    /// Initial temperature: 0 is cold smoke, ~1 is a yellow-white flame.
    pub heat: f32,
    /// Time constant of the cooling, seconds.
    pub cool: f32,
    /// Emissive gain for hot puffs, sparks and glows.
    pub glow: f32,
    pub kind: Kind,
    /// Fraction of the life at which the puff starts to dissolve.
    pub dissolve: f32,
    pub fade_in: f32,
    /// Spark streak length per m/s of speed (seconds of motion blur).
    pub stretch: f32,
    /// Soft-particle fade distance, as a fraction of the size (vapour hugging
    /// the wing wants a small one).
    pub soft: f32,
    /// Simulate in this entity's local space (vapour, muzzle flashes).
    pub frame: Option<Entity>,
    pub tile: f32,
    pub seed: f32,
}

impl Particle {
    fn base(pos: Vec3, kind: Kind) -> Self {
        let mut rng = rand::rng();
        use rand::RngExt;
        Particle {
            pos,
            vel: Vec3::ZERO,
            age: 0.0,
            life: 1.0,
            size: (1.0, 1.0),
            grow: 2.0,
            rot: rng.random_range(0.0..std::f32::consts::TAU),
            spin: rng.random_range(-0.3..0.3),
            drag: 1.0,
            wind: 1.0,
            gravity: 0.0,
            buoyancy: 0.0,
            albedo: Vec3::splat(0.5),
            opacity: 1.0,
            heat: 0.0,
            cool: 1.0,
            glow: 0.0,
            kind,
            dissolve: 0.35,
            fade_in: 0.0,
            stretch: 0.0,
            soft: 1.0,
            frame: None,
            tile: rng.random_range(0..textures::ATLAS_TILES * textures::ATLAS_TILES) as f32,
            seed: rng.random_range(0.0..1.0),
        }
    }

    pub fn puff(pos: Vec3) -> Self {
        Self::base(pos, Kind::Puff)
    }

    pub fn spark(pos: Vec3, vel: Vec3) -> Self {
        Particle {
            vel,
            heat: 1.0,
            cool: 0.5,
            glow: 7.0,
            gravity: 9.8,
            drag: 0.6,
            wind: 0.0,
            size: (0.12, 0.08),
            stretch: 0.018,
            ..Self::base(pos, Kind::Spark)
        }
    }

    pub fn glow(pos: Vec3) -> Self {
        Particle {
            heat: 1.0,
            cool: 1e6,
            glow: 4.0,
            wind: 0.0,
            drag: 0.0,
            ..Self::base(pos, Kind::Glow)
        }
    }

    pub fn heat_now(&self) -> f32 {
        self.heat * (-self.age.max(0.0) / self.cool).exp()
    }
}

/// A quad handed straight to the renderer (trail ribbons).
#[derive(Clone, Copy)]
pub struct Quad {
    pub corners: [Vec3; 4],
    /// x: across the tube 0..1, y: distance along the trail (m)
    pub uv: [Vec2; 4],
    pub albedo: Vec3,
    pub opacity: [f32; 4],
    pub dissolve: f32,
    pub seed: f32,
    /// unit vector across the ribbon (for the tube normal)
    pub side: Vec3,
    pub glow: f32,
    pub heat: f32,
}

#[derive(Resource, Default)]
pub struct Particles {
    pub list: Vec<Particle>,
    /// Ribbon quads for this frame; drained by the mesh builder.
    pub quads: Vec<Quad>,
}

impl Particles {
    /// Adds a particle unless the system is full. Lower priority effects
    /// (`priority` < 1) give up earlier so explosions always have room.
    pub fn add(&mut self, p: Particle, priority: f32) {
        let cap = (MAX_PARTICLES as f32 * priority.clamp(0.0, 1.0)) as usize;
        if self.list.len() < cap.max(MAX_PARTICLES / 2) {
            self.list.push(p);
        }
    }
}

#[derive(Clone, Copy, ShaderType, Default)]
pub struct FxGlobals {
    /// xyz: direction to the sun
    pub sun_dir: Vec4,
    /// rgb tint, w: illuminance (lux)
    pub sun: Vec4,
    /// rgb tint, w: sky illuminance (lux)
    pub ambient: Vec4,
    /// x: soft-particle depth scale
    pub misc: Vec4,
}

#[derive(Asset, TypePath, AsBindGroup, Clone)]
pub struct ParticleMaterial {
    #[uniform(0)]
    pub globals: FxGlobals,
    #[texture(1)]
    #[sampler(2)]
    pub atlas: Handle<Image>,
    #[texture(3)]
    #[sampler(4)]
    pub noise: Handle<Image>,
}

impl Material for ParticleMaterial {
    fn vertex_shader() -> ShaderRef {
        "shaders/fx_particles.wesl".into()
    }
    fn fragment_shader() -> ShaderRef {
        "shaders/fx_particles.wesl".into()
    }
    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Premultiplied
    }
    fn enable_prepass() -> bool {
        false
    }
    fn enable_shadows() -> bool {
        false
    }
    fn specialize(
        _pipeline: &MaterialPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        layout: &MeshVertexBufferLayoutRef,
        _key: MaterialPipelineKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        descriptor.vertex.buffers = vec![layout.0.get_layout(&[
            Mesh::ATTRIBUTE_POSITION.at_shader_location(0),
            Mesh::ATTRIBUTE_UV_0.at_shader_location(1),
            Mesh::ATTRIBUTE_COLOR.at_shader_location(2),
            ATTRIBUTE_FX_A.at_shader_location(3),
            ATTRIBUTE_FX_B.at_shader_location(4),
        ])?];
        descriptor.primitive.cull_mode = None;
        Ok(())
    }
}

#[derive(Resource)]
pub struct ParticleRender {
    mesh: Handle<Mesh>,
    pub material: Handle<ParticleMaterial>,
    pub noise: Handle<Image>,
}

pub fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ParticleMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {
    let start = std::time::Instant::now();
    let atlas = images.add(textures::puff_atlas());
    let noise = images.add(textures::noise());
    debug!("baked effect textures in {:?}", start.elapsed());
    let mesh = meshes.add(empty_mesh());
    let material = materials.add(ParticleMaterial {
        globals: FxGlobals::default(),
        atlas,
        noise: noise.clone(),
    });
    commands.spawn((
        Name::new("particles"),
        Mesh3d(mesh.clone()),
        MeshMaterial3d(material.clone()),
        Transform::IDENTITY,
        Visibility::Hidden,
        NoFrustumCulling,
        bevy::light::NotShadowCaster,
        ParticleMesh,
    ));
    commands.insert_resource(ParticleRender { mesh, material, noise });
    commands.init_resource::<Particles>();
}

#[derive(Component)]
pub struct ParticleMesh;

fn empty_mesh() -> Mesh {
    // a degenerate triangle: empty meshes make the allocator complain
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, vec![[0.0f32; 3]; 3])
        .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, vec![[0.0f32; 2]; 3])
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, vec![[0.0f32; 4]; 3])
        .with_inserted_attribute(ATTRIBUTE_FX_A, vec![[0.0f32; 4]; 3])
        .with_inserted_attribute(ATTRIBUTE_FX_B, vec![[0.0f32; 4]; 3])
        .with_inserted_indices(Indices::U32(vec![0, 1, 2]))
}

/// Keeps the shader's sun and sky in step with the scene's directional light.
pub fn update_globals(
    render: Res<ParticleRender>,
    mut materials: ResMut<Assets<ParticleMaterial>>,
    sun: Query<(&DirectionalLight, &GlobalTransform)>,
    mut last: Local<Option<Vec3>>,
) {
    let Some((light, gt)) = sun.iter().next() else {
        return;
    };
    let dir = gt.back().as_vec3();
    if *last == Some(dir) {
        return;
    }
    *last = Some(dir);
    let Some(mut m) = materials.get_mut(&render.material) else {
        return;
    };
    // the atmosphere dims and warms the sun as it gets lower
    let elevation = dir.y.clamp(0.0, 1.0);
    let transmittance = 0.25 + 0.6 * elevation.powf(0.5);
    let warm = Vec3::new(1.0, 0.93 - 0.25 * (1.0 - elevation), 0.85 - 0.45 * (1.0 - elevation));
    m.globals = FxGlobals {
        sun_dir: dir.extend(0.0),
        sun: warm.extend(light.illuminance * transmittance),
        // skylight: the blue sky lights the shadowed side of smoke
        ambient: Vec3::new(0.62, 0.74, 1.0).extend(light.illuminance * 0.2),
        misc: Vec4::new(1.0, 0.0, 0.0, 0.0),
    };
}

/// Soft particles need the depth prepass on the main camera, heat haze the
/// transmission texture.
pub fn enable_depth_prepass(mut commands: Commands, cameras: Query<Entity, Added<MainCamera>>) {
    for camera in &cameras {
        // the depth prepass feeds soft particles and the plume's occlusion; the
        // transmission copy feeds the heat haze
        commands.entity(camera).insert((
            bevy::core_pipeline::prepass::DepthPrepass,
            bevy::pbr::ScreenSpaceTransmission::default(),
        ));
    }
}

pub fn simulate(
    time: Res<Time>,
    mut particles: ResMut<Particles>,
    frames: Query<(), With<GlobalTransform>>,
) {
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }
    let list = &mut particles.list;
    let mut i = 0;
    while i < list.len() {
        let p = &mut list[i];
        p.age += dt;
        let dead = p.age >= p.life || p.frame.is_some_and(|f| !frames.contains(f));
        if dead {
            list.swap_remove(i);
            continue;
        }
        if p.age >= 0.0 {
            if p.frame.is_none() {
                let k = 1.0 - (-p.drag * dt).exp();
                let air = WIND * p.wind;
                p.vel += (air - p.vel) * k;
                let lift = p.buoyancy * (0.3 + p.heat_now().min(1.0)) * (1.0 - p.age / p.life);
                p.vel.y += (lift - p.gravity) * dt;
            } else {
                p.vel *= (-p.drag * dt).exp();
            }
            p.pos += p.vel * dt;
            p.rot += p.spin * dt;
        }
        i += 1;
    }
}

pub struct Built {
    key: f32,
    corners: [Vec3; 4],
    uv: [Vec2; 4],
    color: [f32; 4],
    opacity: [f32; 4],
    a: [f32; 4],
    b: [f32; 4],
}

const CORNERS: [Vec2; 4] = [
    Vec2::new(-1.0, -1.0),
    Vec2::new(1.0, -1.0),
    Vec2::new(1.0, 1.0),
    Vec2::new(-1.0, 1.0),
];

#[allow(clippy::too_many_arguments)]
pub fn build_mesh(
    mut particles: ResMut<Particles>,
    render: Res<ParticleRender>,
    mut meshes: ResMut<Assets<Mesh>>,
    camera: Query<&GlobalTransform, With<MainCamera>>,
    frames: Query<&GlobalTransform, Without<MainCamera>>,
    mut visibility: Query<&mut Visibility, With<ParticleMesh>>,
    mut built: Local<Vec<Built>>,
    mut order: Local<Vec<(f32, u32)>>,
    time: Res<Time>,
    mut log: Local<(f32, f32, u32)>,
) {
    let started = std::time::Instant::now();
    let Ok(cam) = camera.single() else {
        return;
    };
    let cam_pos = cam.translation();
    let (right, up) = (cam.right().as_vec3(), cam.up().as_vec3());

    built.clear();
    for p in &particles.list {
        if p.age < 0.0 {
            continue;
        }
        let t = (p.age / p.life).clamp(0.0, 1.0);
        let (world, vel) = match p.frame {
            Some(f) => match frames.get(f) {
                Ok(gt) => (gt.transform_point(p.pos), gt.rotation() * p.vel),
                Err(_) => continue,
            },
            None => (p.pos, p.vel),
        };
        let size = p.size.0 + (p.size.1 - p.size.0) * (1.0 - (1.0 - t).powf(p.grow));
        let fade_in = if p.fade_in > 0.0 { (p.age / p.fade_in).min(1.0) } else { 1.0 };
        let dissolve = ((t - p.dissolve) / (1.0 - p.dissolve).max(1e-3)).clamp(0.0, 1.0);
        let heat = p.heat_now();
        let to_cam = cam_pos - world;
        let dist = to_cam.length();
        let (x_axis, y_axis, hx, hy, angle) = match p.kind {
            Kind::Spark => {
                // stretch along the screen-space direction of motion
                let speed = vel.length();
                let dir = vel / speed.max(1e-3);
                let side = dir.cross(to_cam).normalize_or(right);
                let len = (speed * p.stretch).max(size);
                (side, dir, size * 0.5, len * 0.5, 0.0)
            }
            _ => {
                let (s, c) = p.rot.sin_cos();
                (right * c + up * s, up * c - right * s, size * 0.5, size * 0.5, p.rot)
            }
        };
        let mut corners = [Vec3::ZERO; 4];
        let mut uv = [Vec2::ZERO; 4];
        for (k, c) in CORNERS.iter().enumerate() {
            corners[k] = world + x_axis * c.x * hx + y_axis * c.y * hy;
            uv[k] = Vec2::new(0.5 + 0.5 * c.x, 0.5 - 0.5 * c.y);
        }
        let opacity = p.opacity * fade_in;
        built.push(Built {
            key: dist,
            corners,
            uv,
            color: [p.albedo.x, p.albedo.y, p.albedo.z, opacity],
            opacity: [opacity; 4],
            // the tile index carries the softness in its fraction
            a: [heat, dissolve, p.tile + p.soft.clamp(0.0, 1.0) * 0.49, p.seed],
            b: [p.kind as u32 as f32, size, p.glow, angle],
        });
    }
    for q in particles.quads.drain(..) {
        let centre = (q.corners[0] + q.corners[1] + q.corners[2] + q.corners[3]) * 0.25;
        // angle of the across-ribbon direction in the camera plane
        let angle = q.side.dot(up).atan2(q.side.dot(right));
        let width = (q.corners[1] - q.corners[0]).length();
        built.push(Built {
            key: centre.distance(cam_pos),
            corners: q.corners,
            uv: q.uv,
            color: [q.albedo.x, q.albedo.y, q.albedo.z, 1.0],
            opacity: q.opacity,
            a: [q.heat, q.dissolve, 0.49, q.seed],
            b: [Kind::Tube as u32 as f32, width, q.glow, angle],
        });
    }

    // stay visible even when empty, so the pipeline is compiled up front
    // rather than hitching on the first explosion
    if let Ok(mut vis) = visibility.single_mut() {
        *vis = Visibility::Inherited;
    }
    if built.is_empty() {
        if let Some(mut mesh) = meshes.get_mut(&render.mesh)
            && mesh.count_vertices() != 3
        {
            *mesh = empty_mesh();
        }
        return;
    }

    order.clear();
    order.extend(built.iter().enumerate().map(|(i, b)| (b.key, i as u32)));
    // back to front
    order.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));

    let n = order.len();
    let mut positions = Vec::with_capacity(n * 4);
    let mut uvs = Vec::with_capacity(n * 4);
    let mut colors = Vec::with_capacity(n * 4);
    let mut fx_a = Vec::with_capacity(n * 4);
    let mut fx_b = Vec::with_capacity(n * 4);
    let mut indices = Vec::with_capacity(n * 6);
    for (slot, &(_, i)) in order.iter().enumerate() {
        let b = &built[i as usize];
        for k in 0..4 {
            positions.push(b.corners[k].to_array());
            uvs.push(b.uv[k].to_array());
            colors.push([b.color[0], b.color[1], b.color[2], b.opacity[k]]);
            fx_a.push(b.a);
            fx_b.push(b.b);
        }
        let v = (slot * 4) as u32;
        indices.extend_from_slice(&[v, v + 1, v + 2, v, v + 2, v + 3]);
    }
    if let Some(mut mesh) = meshes.get_mut(&render.mesh) {
        mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
        mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
        mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
        mesh.insert_attribute(ATTRIBUTE_FX_A, fx_a);
        mesh.insert_attribute(ATTRIBUTE_FX_B, fx_b);
        mesh.insert_indices(Indices::U32(indices));
    }
    // `FLIGHT_SIM_LOG=1`: particle count and the CPU cost of building the mesh
    log.1 += started.elapsed().as_secs_f32() * 1000.0;
    log.2 += 1;
    if time.elapsed_secs() - log.0 >= 1.0 && std::env::var("FLIGHT_SIM_LOG").is_ok() {
        info!(
            "particles {} quads, mesh build {:.2} ms/frame",
            n,
            log.1 / log.2.max(1) as f32
        );
        *log = (time.elapsed_secs(), 0.0, 0);
    }
}
