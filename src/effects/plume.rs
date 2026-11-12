//! Engine exhaust: a raymarched volumetric plume (shock diamonds, hot core,
//! turbulent outer flame, nozzle glow) and a screen-space heat-haze shell.
//!
//! Both are cylinders in plume space: the mesh's local +Y runs down the
//! exhaust, y = -`HALF_LENGTH` sits at the nozzle mouth. Their transforms must
//! not be scaled (the shaders invert them as rigid transforms).

use bevy::{
    mesh::MeshVertexBufferLayoutRef,
    pbr::{MaterialPipeline, MaterialPipelineKey},
    prelude::*,
    render::render_resource::{
        AsBindGroup, CompareFunction, Face, RenderPipelineDescriptor, ShaderType,
        SpecializedMeshPipelineError,
    },
    asset::uuid_handle,
    shader::{ShaderImport, ShaderRef},
};

/// The plume and haze shaders import `mesh_functions`, which needs
/// `mesh_bindings`. That module is registered when the renderer starts, which
/// on the web is async and can land after our own shader files: compose them
/// too early and the pipelines are broken for good. So they're embedded and
/// only added once `mesh_bindings` exists (`install_shaders`).
const PLUME_SHADER: Handle<Shader> = uuid_handle!("6c0f4d0e-3c1b-4f53-9d8e-2a9a7e1f5b01");
const HAZE_SHADER: Handle<Shader> = uuid_handle!("6c0f4d0e-3c1b-4f53-9d8e-2a9a7e1f5b02");

pub fn install_shaders(mut shaders: ResMut<Assets<Shader>>, mut done: Local<bool>) {
    if *done {
        return;
    }
    let ready = shaders.iter().any(|(_, s)| {
        matches!(&s.import_path, ShaderImport::Custom(p) if p == "bevy_pbr::render::mesh_bindings")
    });
    if !ready {
        return;
    }
    *done = true;
    let plume = Shader::from_wesl(include_str!("../../assets/shaders/fx_plume.wesl"), "shaders/fx_plume.wesl");
    let haze = Shader::from_wesl(include_str!("../../assets/shaders/fx_haze.wesl"), "shaders/fx_haze.wesl");
    let _ = shaders.insert(PLUME_SHADER.id(), plume);
    let _ = shaders.insert(HAZE_SHADER.id(), haze);
}

pub const HALF_LENGTH: f32 = 7.0;
pub const RADIUS: f32 = 1.15;
pub const HAZE_HALF_LENGTH: f32 = 10.0;
pub const HAZE_RADIUS: f32 = 2.2;

#[derive(Clone, Copy, ShaderType, Default, Debug)]
pub struct PlumeParams {
    /// x: dry power 0..1, y: afterburner 0..1, z: seed, w: length flicker
    pub state: Vec4,
    /// x: half length, y: radius, z: nozzle half width, w: nozzle half height
    pub shape: Vec4,
}

#[derive(Asset, TypePath, AsBindGroup, Clone)]
pub struct PlumeMaterial {
    #[uniform(0)]
    pub params: PlumeParams,
    #[texture(1)]
    #[sampler(2)]
    pub noise: Handle<Image>,
}

impl Material for PlumeMaterial {
    fn vertex_shader() -> ShaderRef {
        PLUME_SHADER.into()
    }
    fn fragment_shader() -> ShaderRef {
        PLUME_SHADER.into()
    }
    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Add
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
        descriptor.vertex.buffers =
            vec![layout.0.get_layout(&[Mesh::ATTRIBUTE_POSITION.at_shader_location(0)])?];
        // draw the far side of the hull and do the depth test ourselves against
        // the prepass while marching, so the plume is cut exactly by the airframe
        descriptor.primitive.cull_mode = Some(Face::Front);
        if let Some(depth) = descriptor.depth_stencil.as_mut() {
            depth.depth_compare = Some(CompareFunction::Always);
            depth.depth_write_enabled = Some(false);
        }
        Ok(())
    }
}

#[derive(Asset, TypePath, AsBindGroup, Clone)]
pub struct HazeMaterial {
    #[uniform(0)]
    pub params: PlumeParams,
    #[texture(1)]
    #[sampler(2)]
    pub noise: Handle<Image>,
}

impl Material for HazeMaterial {
    fn vertex_shader() -> ShaderRef {
        HAZE_SHADER.into()
    }
    fn fragment_shader() -> ShaderRef {
        HAZE_SHADER.into()
    }
    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Blend
    }
    fn reads_view_transmission_texture(&self) -> bool {
        true
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
        descriptor.vertex.buffers =
            vec![layout.0.get_layout(&[Mesh::ATTRIBUTE_POSITION.at_shader_location(0)])?];
        descriptor.primitive.cull_mode = Some(Face::Back);
        if let Some(depth) = descriptor.depth_stencil.as_mut() {
            depth.depth_write_enabled = Some(false);
        }
        Ok(())
    }
}

/// Smooth 1D value noise in [0, 1] (for flicker that isn't a sine wave).
pub fn noise1(x: f32) -> f32 {
    let h = |i: f32| {
        let v = (i * 127.1).sin() * 43_758.547;
        v - v.floor()
    };
    let i = x.floor();
    let f = x - i;
    let u = f * f * (3.0 - 2.0 * f);
    h(i) + (h(i + 1.0) - h(i)) * u
}

/// Turbulent flicker around 1.0: a few octaves of value noise.
pub fn turbulence(t: f32, seed: f32) -> f32 {
    let n = noise1(t * 9.0 + seed) * 0.5 + noise1(t * 23.0 + seed * 3.0) * 0.3 + noise1(t * 51.0 + seed * 7.0) * 0.2;
    0.8 + 0.4 * n
}
