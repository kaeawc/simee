//! The compositor: with the 3D view off it draws Rune's default frame (`Frame::draw_ui`), so the
//! window is what it is without one. With it on it draws the frame through
//! `Renderer::draw_with_backdrop`; at the scene's slot, while the pass is closed, it renders the
//! body into its own 4x MSAA colour and depth targets (Rune's passes have neither), sampling the
//! flat screen's rect of the backdrop copy for the live screen, then draws the resolved picture
//! into the slot. The body is shaded as glass and metal under a studio fixed to the camera, over
//! a floor that carries its contact shadow (scene.wgsl; the materials are `layout::material`). Nothing is cached between frames: the window draws only when something changed
//! (a device frame, input, a resize), so a still scene costs nothing.

use std::num::NonZeroU64;
use std::sync::Arc;

use rune::compose::{Compositor, DeviceKey, Frame};
use rune::render::{EffectPhase, EffectSite};
use sim_core::body::{Body, Face, Mesh, Role};

use super::camera::{self, Camera, Framing, Mat4, Turn};
use super::layout::{self, STRIDE};
use super::{MARK, Placement};
use crate::State;

const SAMPLES: u32 = 4;
const COLOR: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const DEPTH: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
/// Bytes of the frame uniform (`Frame` in scene.wgsl) and of one draw's (`Draw`).
const FRAME_BYTES: u64 = 208;
const DRAW_BYTES: u64 = 96;
/// How dark the contact shadow is at its middle (a share of the stage's colour taken away), how
/// far it fades beyond the body's footprint and how far under the body the floor lies, the last
/// two as shares of the body's height.
const SHADOW: f32 = 0.62;
const SHADOW_BLUR: f32 = 0.16;
const FLOOR_GAP: f32 = 0.012;

/// What one frame of the scene draws, read from the state before Rune's passes borrow it.
struct Job {
    body: Arc<Body>,
    mesh: Arc<Mesh>,
    placement: Placement,
    camera: Camera,
    turn: Turn,
    /// The screen shown faces away from the front once the hinge is set (a book's cover screen
    /// folded shut, any back-face screen); a flip's cover screen on its lid faces the front shut.
    back: bool,
}

/// One frame's camera and the body's turn, solved for its slot.
struct Solved {
    view_proj: Mat4,
    eye: [f32; 3],
    /// The device's [`Turn`] about the body's centre, applied after its placement.
    turn: Mat4,
    /// Where the live screen's corners land, slot device pixels, when it faces the camera.
    screen: Option<[[f32; 2]; 4]>,
    /// The box the turned body fills (its centre and half size, world): what the floor lies under.
    body: ([f32; 3], [f32; 3]),
}

impl Job {
    fn from_state(s: &State) -> Option<Job> {
        let body = super::active(s)?;
        let placement = super::place(&body, s.screen_size(), s.screen_rotation())?;
        let mesh = sim_core::body::cached_mesh(&body);
        let back = facing_back(&body, &placement);
        let view = &s.view3d.view;
        Some(Job { body, mesh, placement, camera: view.camera_now(), turn: view.turn_now(), back })
    }

    /// The box the live screen's face fills, placed (without the turn).
    fn screen_box(&self, root: &Mat4) -> ([f32; 3], [f32; 3]) {
        let models = [self.panel(root, 0), self.panel(root, 1)];
        let mesh = &self.mesh;
        let live = sim_core::body::Role::Screen(self.placement.display);
        let points = mesh.parts.iter().filter(|p| p.role == live).flat_map(move |part| {
            let m = models[part.panel.min(1)];
            mesh.part_positions(part).iter().map(move |&p| camera::apply(&m, p))
        });
        camera::bounds(points)
    }

    /// The camera and turn for a slot of `size` device pixels whose flat screen is `flat` (its
    /// centre off the slot's centre and its height, device pixels).
    fn solve(&self, size: (u32, u32), flat: [f32; 3]) -> Solved {
        let root = self.root();
        let (center, placed) = self.bounds(&root);
        let (screen, screen_half) = self.screen_box(&root);
        let turn = self.turn.matrix(center);
        // The fit is the turned body's box: a spun phone lies across the slot and fits that way.
        let half = if self.turn == Turn::default() { placed } else { self.bounds(&camera::mul(&turn, &root)).1 };
        let framing = Framing { center, half, screen: camera::apply(&turn, screen), screen_h: 2.0 * screen_half[1], flat };
        let (view_proj, eye) = self.camera.solve(&framing, size, self.back);
        // The screen's corners on its face's plane, and whether that face looks at the eye.
        let normal = camera::turn(&turn, [0.0, 0.0, if self.back { -1.0 } else { 1.0 }]);
        let to_eye: [f32; 3] = std::array::from_fn(|k| eye[k] - framing.screen[k]);
        let facing = (0..3).map(|k| normal[k] * to_eye[k]).sum::<f32>() > 0.0;
        let (w, h) = (size.0 as f32, size.1 as f32);
        let corners = [[-1.0, 1.0], [1.0, 1.0], [1.0, -1.0], [-1.0, -1.0]].map(|[sx, sy]: [f32; 2]| {
            let p = camera::apply(&turn, [screen[0] + sx * screen_half[0], screen[1] + sy * screen_half[1], screen[2]]);
            camera::apply(&view_proj, p)
        });
        let in_front = corners.iter().all(|c| (0.0..=1.0).contains(&c[2]));
        let screen = (facing && in_front).then(|| corners.map(|[x, y, _]| [(x + 1.0) / 2.0 * w, (1.0 - y) / 2.0 * h]));
        Solved { view_proj, eye, turn, screen, body: (center, half) }
    }

    /// The body's roll about z: clockwise as its screen is seen (from the back for a back-face
    /// screen).
    fn root(&self) -> Mat4 {
        let quarter = -std::f32::consts::FRAC_PI_2 * self.placement.quarters as f32;
        camera::rot_z(if self.back { -quarter } else { quarter })
    }

    /// Each panel's transform: panel 1 turns about the hinge.
    fn panel(&self, root: &Mat4, panel: usize) -> Mat4 {
        match (&self.body.fold, panel) {
            (Some(f), 1) => camera::mul(root, &f.hinge.transform(self.placement.hinge)),
            _ => *root,
        }
    }

    /// The box the drawn body fills (every vertex under its panel's transform): what the camera
    /// frames, a folded foldable's stacked panels included.
    fn bounds(&self, root: &Mat4) -> ([f32; 3], [f32; 3]) {
        let models = [self.panel(root, 0), self.panel(root, 1)];
        let mesh = &self.mesh;
        // Each vertex once: a part's positions, and a panel's skin (three parts on the same
        // vertices) by its band alone.
        let points = mesh.parts.iter().filter(|p| !matches!(p.role, Role::Glass | Role::Back)).flat_map(move |part| {
            let m = models[part.panel.min(1)];
            mesh.part_positions(part).iter().map(move |&p| camera::apply(&m, p))
        });
        camera::bounds(points)
    }
}

/// Whether the screen `placement` shows faces -z with the hinge where it puts it: its face's
/// normal turned by its panel's transform.
pub fn facing_back(body: &Body, placement: &Placement) -> bool {
    let Some(screen) = body.screen(placement.display) else { return false };
    let z = if screen.face == Face::Front { 1.0 } else { -1.0 };
    let m = match &body.fold {
        Some(f) if body.panel_at(screen.center) == 1 => f.hinge.transform(placement.hinge),
        _ => return z < 0.0,
    };
    // The normal's z after the panel's turn: the third column's z.
    m[2][2] * z < 0.0
}

/// The floor under a body and the footprint it casts there.
struct Floor {
    /// The unit quad (`layout::floor_quad`) scaled over the shadow and set at the floor's height.
    model: Mat4,
    /// Centre x and z, half width and half depth of the body's box on the floor, mm.
    foot: [f32; 4],
    /// How far the shadow fades beyond the footprint, mm.
    blur: f32,
}

impl Floor {
    /// The floor for a body whose box is `half` about `center` (world, mm): just under it, with
    /// a quad that reaches as far as the shadow does.
    fn under(center: [f32; 3], half: [f32; 3]) -> Floor {
        let height = 2.0 * half[1];
        let blur = (SHADOW_BLUR * height).max(1.0);
        let y = center[1] - half[1] - FLOOR_GAP * height;
        let (sx, sz) = (half[0] + blur, half[2] + blur);
        let model = [[sx, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, sz, 0.0], [center[0], y, center[2], 1.0]];
        // The footprint is drawn in a little from the box: a body's corners are rounded.
        Floor { model, foot: [center[0], center[2], 0.94 * half[0], 0.94 * half[2]], blur }
    }
}

/// The 3D view's compositor (see the module docs).
#[derive(Default)]
pub struct Scene {
    gpu: Option<Gpu>,
    drew: bool,
}

impl Compositor<State> for Scene {
    fn render(&mut self, f: &mut Frame<'_, State>) {
        let Some(job) = Job::from_state(&f.rt.state) else {
            self.drew = false;
            // 3D is off: the MSAA, depth and resolve targets (~107 MB at 2x) go until it is on again.
            if let Some(gpu) = self.gpu.as_mut() {
                gpu.targets = None;
            }
            f.draw_ui();
            return;
        };
        let key = f.device_key();
        if self.gpu.as_ref().is_none_or(|g| g.key != key) {
            self.gpu = Some(Gpu::new(f.device, key));
        }
        let (device, queue) = (f.device, f.queue);
        let clear = f.rt.clear_color();
        f.ui.set_fade_stops(f.rt.fade_stops());
        f.ui.prepare(device, queue, &mut f.rt.text.atlas, &f.rt.instances, f.size);
        let Some(gpu) = self.gpu.as_mut() else { return };
        gpu.ready = false;
        let mut screen = None;
        f.ui.draw_with_backdrop(f.encoder, f.target, clear, &mut |phase, site| {
            if site.mark != MARK {
                return;
            }
            match phase {
                EffectPhase::Backdrop(encoder) => screen = gpu.draw_scene(device, queue, encoder, site, &job),
                EffectPhase::Draw(pass) => gpu.blit(device, queue, pass, site),
            }
        });
        self.drew = gpu.ready;
        // Where the screen was drawn, for the next press to tell the camera's drags from the
        // device's.
        f.rt.state.view3d.view.screen_quad = screen;
    }

    fn draws_own(&self) -> bool {
        self.drew
    }

    fn release_gpu(&mut self) {
        self.gpu = None;
        self.drew = false;
    }
}

struct Targets {
    size: (u32, u32),
    msaa: wgpu::TextureView,
    depth: wgpu::TextureView,
    resolve: wgpu::TextureView,
    blit: wgpu::BindGroup,
}

struct MeshBuffers {
    id: String,
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    /// Where the floor's quad starts in the index buffer, after the body's own indices.
    floor: u32,
}

struct Gpu {
    key: DeviceKey,
    pipeline: wgpu::RenderPipeline,
    frame_layout: wgpu::BindGroupLayout,
    frame_u: wgpu::Buffer,
    draw_layout: wgpu::BindGroupLayout,
    draws: Option<(usize, wgpu::Buffer, wgpu::BindGroup)>,
    draw_stride: u64,
    sampler: wgpu::Sampler,
    mesh: Option<MeshBuffers>,
    targets: Option<Targets>,
    blit_layout: wgpu::BindGroupLayout,
    blit_pipeline_layout: wgpu::PipelineLayout,
    blit_module: wgpu::ShaderModule,
    blits: Vec<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
    blit_u: wgpu::Buffer,
    /// The scene was drawn this frame, so the slot shows it.
    ready: bool,
}

fn uniform_entry(binding: u32, dynamic: bool, size: u64) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: dynamic,
            min_binding_size: NonZeroU64::new(size),
        },
        count: None,
    }
}

/// A uniform, a filterable 2D texture and a sampler: the scene's frame group and the blit's.
fn textured_layout(device: &wgpu::Device, uniform: u64) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("simee scene textured"),
        entries: &[
            uniform_entry(0, false, uniform),
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    })
}

fn uniform_buffer(device: &wgpu::Device, label: &str, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn bytes(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn flat(m: &Mat4) -> impl Iterator<Item = f32> + '_ {
    m.iter().flatten().copied()
}

impl Gpu {
    fn new(device: &wgpu::Device, key: DeviceKey) -> Gpu {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("simee scene"),
            source: wgpu::ShaderSource::Wgsl(include_str!("scene.wgsl").into()),
        });
        let frame_layout = textured_layout(device, FRAME_BYTES);
        let draw_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("simee scene draw"),
            entries: &[uniform_entry(0, true, DRAW_BYTES)],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("simee scene"),
            bind_group_layouts: &[Some(&frame_layout), Some(&draw_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("simee scene"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: STRIDE as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x2],
                })],
            },
            // Lit from both sides, so no face can vanish to a winding slip.
            primitive: wgpu::PrimitiveState { cull_mode: None, ..Default::default() },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState { count: SAMPLES, ..Default::default() },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(COLOR.into())],
            }),
            multiview_mask: None,
            cache: None,
        });
        let align = u64::from(device.limits().min_uniform_buffer_offset_alignment).max(1);
        let blit_layout = textured_layout(device, 48);
        let blit_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("simee scene blit"),
            bind_group_layouts: &[Some(&blit_layout)],
            immediate_size: 0,
        });
        let blit_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("simee scene blit"),
            source: wgpu::ShaderSource::Wgsl(include_str!("blit.wgsl").into()),
        });
        Gpu {
            key,
            pipeline,
            frame_u: uniform_buffer(device, "simee scene frame", FRAME_BYTES),
            frame_layout,
            draw_layout,
            draws: None,
            draw_stride: DRAW_BYTES.div_ceil(align) * align,
            sampler: device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("simee scene"),
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            }),
            mesh: None,
            targets: None,
            blit_layout,
            blit_pipeline_layout,
            blit_module,
            blits: Vec::new(),
            blit_u: uniform_buffer(device, "simee scene blit", 48),
            ready: false,
        }
    }

    /// The MSAA colour and depth targets and the resolve texture, at the slot's size.
    fn targets(&mut self, device: &wgpu::Device, size: (u32, u32)) {
        if self.targets.as_ref().is_some_and(|t| t.size == size) {
            return;
        }
        let texture = |samples, format, usage| {
            device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("simee scene"),
                    size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: samples,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage,
                    view_formats: &[],
                })
                .create_view(&Default::default())
        };
        let msaa = texture(SAMPLES, COLOR, wgpu::TextureUsages::RENDER_ATTACHMENT);
        let depth = texture(SAMPLES, DEPTH, wgpu::TextureUsages::RENDER_ATTACHMENT);
        let resolve = texture(1, COLOR, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING);
        let blit = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("simee scene blit"),
            layout: &self.blit_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.blit_u.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&resolve) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        self.targets = Some(Targets { size, msaa, depth, resolve, blit });
    }

    /// The body's vertex and index buffers, made once per body.
    fn mesh(&mut self, device: &wgpu::Device, job: &Job) {
        if self.mesh.as_ref().is_some_and(|m| m.id == job.body.id) {
            return;
        }
        let buffer = |label, data: Vec<u8>, usage| {
            use wgpu::util::DeviceExt;
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some(label), contents: &data, usage })
        };
        // The floor's quad rides at the end of the body's buffers.
        let (mut vertices, mut indices) = (layout::vertices(&job.mesh), layout::indices(&job.mesh));
        let (quad_v, quad_i) = layout::floor_quad(job.mesh.positions.len() as u32);
        vertices.extend(quad_v);
        indices.extend(quad_i);
        self.mesh = Some(MeshBuffers {
            id: job.body.id.clone(),
            vertices: buffer("simee body vertices", vertices, wgpu::BufferUsages::VERTEX),
            indices: buffer("simee body indices", indices, wgpu::BufferUsages::INDEX),
            floor: job.mesh.indices.len() as u32,
        });
    }

    /// Room for `n` draws' uniforms.
    fn draws(&mut self, device: &wgpu::Device, n: usize) {
        if self.draws.as_ref().is_some_and(|(cap, ..)| *cap >= n) {
            return;
        }
        let cap = n.max(32).next_power_of_two();
        let buffer = uniform_buffer(device, "simee scene draws", cap as u64 * self.draw_stride);
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("simee scene draws"),
            layout: &self.draw_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &buffer,
                    offset: 0,
                    size: NonZeroU64::new(DRAW_BYTES),
                }),
            }],
        });
        self.draws = Some((cap, buffer, group));
    }

    /// Renders the body into the MSAA targets and resolves it, with the screen sampled from
    /// `site.backdrop` (the frame up to the slot, the flat screen included). Returns where the
    /// live screen was drawn (slot points), when it faces the camera.
    fn draw_scene(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        site: &EffectSite<'_>,
        job: &Job,
    ) -> Option<[[f32; 2]; 4]> {
        let backdrop = site.backdrop?;
        let r = site.instance.rect;
        let size = ((r[2].round() as u32).max(1), (r[3].round() as u32).max(1));
        self.targets(device, size);
        self.mesh(device, job);
        // One draw per part, and the floor.
        let parts = job.mesh.parts.len();
        self.draws(device, parts + 1);
        let (Some(t), Some(mesh), Some((_, draw_u, draw_group))) = (&self.targets, &self.mesh, &self.draws) else { return None };

        let root = job.root();
        let [x, y, w, h] = super::screen_rect(r, site.scale, site.data);
        let flat_screen = [x + w / 2.0 - (r[0] + r[2] / 2.0), y + h / 2.0 - (r[1] + r[3] / 2.0), h];
        let Solved { view_proj, eye, turn, screen, body: (center, half) } = job.solve(size, flat_screen);
        // The studio is fixed to the camera: its axes in the world.
        let axes = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]].map(|v| job.camera.world(v, job.back));
        // The floor lies just under the turned body, which casts its footprint on it.
        let floor = Floor::under(center, half);
        let bg = super::background();
        // The glass's reflection leaves the live screen as the view comes face on (1:1).
        let sheen = (1.0 - job.camera.one_to_one).clamp(0.0, 1.0);
        let bd = backdrop.texture();
        let (bw, bh) = (bd.width().max(1) as f32, bd.height().max(1) as f32);
        let (x, y) = (x - site.origin[0], y - site.origin[1]);
        let mut frame: Vec<f32> = flat(&view_proj).collect();
        frame.extend([eye[0], eye[1], eye[2], sheen]);
        for a in axes {
            frame.extend([a[0], a[1], a[2], 0.0]);
        }
        frame.extend([x / bw, y / bh, w / bw, h / bh, if site.srgb_blend { 1.0 } else { 0.0 }, job.placement.quarters as f32, 0.0, 0.0]);
        frame.extend([bg.r, bg.g, bg.b, SHADOW]);
        frame.extend(floor.foot);
        queue.write_buffer(&self.frame_u, 0, &bytes(&frame));
        let mut draws = vec![0u8; (parts + 1) * self.draw_stride as usize];
        let mut put = |i: usize, model: &Mat4, m: &layout::Material, extra: f32| {
            let mut d: Vec<f32> = flat(model).collect();
            d.extend([m.base[0], m.base[1], m.base[2], m.kind.code(), m.metallic, m.roughness, m.reflectance, extra]);
            let at = i * self.draw_stride as usize;
            draws[at..at + DRAW_BYTES as usize].copy_from_slice(&bytes(&d));
        };
        for (i, part) in job.mesh.parts.iter().enumerate() {
            let m = layout::material(part.role, job.placement.display, &job.body.finish);
            put(i, &camera::mul(&turn, &job.panel(&root, part.panel)), &m, 0.0);
        }
        put(parts, &floor.model, &layout::FLOOR, floor.blur);
        queue.write_buffer(draw_u, 0, &draws);
        let frame_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("simee scene frame"),
            layout: &self.frame_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.frame_u.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(backdrop) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        let [cr, cg, cb] = [bg.r, bg.g, bg.b].map(f64::from);
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("simee scene"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &t.msaa,
                depth_slice: None,
                resolve_target: Some(&t.resolve),
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color { r: cr, g: cg, b: cb, a: 1.0 }),
                    store: wgpu::StoreOp::Discard,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &t.depth,
                depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &frame_group, &[]);
        pass.set_vertex_buffer(0, mesh.vertices.slice(..));
        pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
        for (i, part) in job.mesh.parts.iter().enumerate() {
            pass.set_bind_group(1, draw_group, &[(i as u64 * self.draw_stride) as u32]);
            pass.draw_indexed(part.first..part.first + part.count, 0, 0..1);
        }
        // The floor last: the body has already written its depth over it.
        pass.set_bind_group(1, draw_group, &[(parts as u64 * self.draw_stride) as u32]);
        pass.draw_indexed(mesh.floor..mesh.floor + 6, 0, 0..1);
        drop(pass);
        self.ready = true;
        let scale = site.scale.max(f32::EPSILON);
        screen.map(|q| q.map(|[x, y]| [x / scale, y / scale]))
    }

    /// Draws the resolved scene over the slot's rect.
    fn blit(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, pass: &mut wgpu::RenderPass<'_>, site: &EffectSite<'_>) {
        if !self.ready {
            return;
        }
        let Some(t) = &self.targets else { return };
        if !self.blits.iter().any(|(f, _)| *f == site.format) {
            let p = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("simee scene blit"),
                layout: Some(&self.blit_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &self.blit_module,
                    entry_point: Some("blit_vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &self.blit_module,
                    entry_point: Some("blit_fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format: site.format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                }),
                multiview_mask: None,
                cache: None,
            });
            self.blits.push((site.format, p));
        }
        let Some((_, pipeline)) = self.blits.iter().find(|(f, _)| *f == site.format) else { return };
        let r = site.instance.rect;
        let u = [
            r[0],
            r[1],
            r[2],
            r[3],
            site.origin[0],
            site.origin[1],
            site.size[0] as f32,
            site.size[1] as f32,
            if site.srgb_blend { 1.0 } else { 0.0 },
            0.0,
            0.0,
            0.0,
        ];
        queue.write_buffer(&self.blit_u, 0, &bytes(&u));
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &t.blit, &[]);
        pass.draw(0..4, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::camera::IDENTITY;
    use super::*;

    #[test]
    fn the_camera_faces_the_screen_shown_with_the_hinge_set() {
        let fold = sim_core::body::get("pixel_fold").unwrap();
        let at = |display, hinge| Placement { display, face: fold.screen(display).unwrap().face, quarters: 0, hinge };
        assert!(!facing_back(&fold, &at(sim_core::body::Display::Inner, 180.0)));
        // Shut, a book's cover screen is on panel 0's back.
        assert!(facing_back(&fold, &at(sim_core::body::Display::Outer, 0.0)));
        // A flip (an AVD's horizontal hinge): its cover screen is on the lid's back, which faces
        // the front once shut.
        let config: std::collections::HashMap<&str, &str> = [
            ("tag.id", "google_apis"),
            ("hw.lcd.width", "1080"),
            ("hw.lcd.height", "2640"),
            ("hw.lcd.density", "420"),
            ("hw.sensor.hinge", "yes"),
            ("hw.sensor.hinge.type", "0"),
            ("hw.sensor.hinge.sub_type", "0"),
            ("hw.sensor.hinge.ranges", "0-180"),
            ("hw.sensor.hinge.defaults", "180"),
            ("hw.displayRegion.0.1.xOffset", "0"),
            ("hw.displayRegion.0.1.yOffset", "0"),
            ("hw.displayRegion.0.1.width", "720"),
            ("hw.displayRegion.0.1.height", "748"),
        ]
        .into_iter()
        .collect();
        let get = |k: &str| config.get(k).map(|v| (*v).to_owned());
        let profile = sim_core::form_factor::android_profile(get);
        let flip = sim_core::body::for_android_hinge("flip", &profile, sim_core::fold::Hinge::from_config(get).as_ref()).unwrap();
        let shut = super::super::place(&flip, (720, 748), sim_core::rotation::Rotation::Deg0).unwrap();
        assert_eq!((shut.display, shut.hinge), (sim_core::body::Display::Outer, 0.0));
        assert!(!facing_back(&flip, &shut));
        let phone = sim_core::body::get("iPhone-17").unwrap();
        assert!(!facing_back(&phone, &Placement { display: sim_core::body::Display::Main, face: Face::Front, quarters: 0, hinge: 180.0 }));
    }

    #[test]
    fn uniform_layouts_match_the_shader() {
        // view_proj, eye, the camera's three axes, uv_rect, flags, stage, foot.
        assert_eq!(FRAME_BYTES, 64 + 9 * 16);
        // model, base, params.
        assert_eq!(DRAW_BYTES, 64 + 2 * 16);
        assert_eq!(bytes(&[1.0, -2.0]).len(), 8);
        assert_eq!(flat(&IDENTITY).count(), 16);
    }

    #[test]
    fn the_floor_lies_under_the_body_and_reaches_as_far_as_its_shadow() {
        let f = Floor::under([10.0, 5.0, -2.0], [36.0, 75.0, 4.0]);
        // Under the box's bottom (5 - 75), by a little.
        let y = f.model[3][1];
        assert!(y < -70.0 && y > -73.0, "{y}");
        assert_eq!((f.model[3][0], f.model[3][2]), (10.0, -2.0));
        // The quad covers the footprint and the blur beyond it; the footprint is inside the box.
        assert_eq!(f.blur, SHADOW_BLUR * 150.0);
        assert_eq!((f.model[0][0], f.model[2][2]), (36.0 + f.blur, 4.0 + f.blur));
        assert!(f.foot[2] < 36.0 && f.foot[3] < 4.0 && f.foot[..2] == [10.0, -2.0]);
        // A body with no height still has a shadow of some width.
        assert!(Floor::under([0.0; 3], [1.0, 0.0, 1.0]).blur >= 1.0);
    }

    /// An iPhone 17 showing its frame upright, with `camera` and `turn`.
    fn phone(camera: Camera, turn: Turn) -> Job {
        let body = sim_core::body::get("iPhone-17").unwrap();
        let placement = super::super::place(&body, (1206, 2622), sim_core::rotation::Rotation::Deg0).unwrap();
        let mesh = sim_core::body::cached_mesh(&body);
        Job { back: facing_back(&body, &placement), body, mesh, placement, camera, turn }
    }

    /// A 1700x1500 px slot (850x750 points at 2x) whose flat screen is the one the view lays out
    /// there: 1404 px high (the stage's 48-point bezel band each side), centred.
    const SLOT: (u32, u32) = (1700, 1500);
    fn flat_rect() -> [f32; 4] {
        super::super::screen_rect([0.0, 0.0, SLOT.0 as f32, SLOT.1 as f32], 2.0, [24.0, 1206.0 / 2622.0, 0.0, 0.0])
    }

    #[test]
    fn face_on_draws_the_real_screen_over_the_flat_one_pixel_for_pixel() {
        let [x, y, w, h] = flat_rect();
        let job = phone(Camera::preset(super::super::Preset::Face), Turn::default());
        let flat = [x + w / 2.0 - SLOT.0 as f32 / 2.0, y + h / 2.0 - SLOT.1 as f32 / 2.0, h];
        let q = job.solve(SLOT, flat).screen.expect("face on, the screen faces the camera");
        // Its corners are the flat screen's, to a hundredth of a pixel: one backdrop pixel per
        // window pixel, so it samples the flat view's picture exactly.
        let want = [[x, y], [x + w, y], [x + w, y + h], [x, y + h]];
        for (got, want) in q.iter().zip(want) {
            assert!((got[0] - want[0]).abs() < 0.01 && (got[1] - want[1]).abs() < 0.01, "{q:?} vs {want:?}");
        }
    }

    #[test]
    fn the_drawn_screen_follows_the_camera_and_is_gone_when_flipped() {
        let [x, y, w, h] = flat_rect();
        let flat = [x + w / 2.0 - SLOT.0 as f32 / 2.0, y + h / 2.0 - SLOT.1 as f32 / 2.0, h];
        let three = phone(Camera::preset(super::super::Preset::ThreeQuarter), Turn::default()).solve(SLOT, flat);
        let q = three.screen.expect("the 3/4 view sees the screen");
        // Fitted, the screen is smaller than the flat one and inside the slot.
        assert!(q.iter().all(|p| p[0] > 0.0 && p[0] < SLOT.0 as f32 && p[1] > 0.0 && p[1] < SLOT.1 as f32), "{q:?}");
        assert!(q[3][1] - q[0][1] < h);
        let flipped = Turn::default().flip();
        assert_eq!(phone(Camera::preset(super::super::Preset::Face), flipped).solve(SLOT, flat).screen, None, "its back faces us");
        assert_eq!(phone(Camera::preset(super::super::Preset::Back), Turn::default()).solve(SLOT, flat).screen, None);
        // Flipped and seen from behind, it faces us again.
        assert!(phone(Camera::preset(super::super::Preset::Back), flipped).solve(SLOT, flat).screen.is_some());
        // Spun a quarter clockwise, the screen's top edge is on the right.
        let spun = phone(Camera::preset(super::super::Preset::Front), Turn::default().spin_by(1)).solve(SLOT, flat);
        let q = spun.screen.unwrap();
        assert!(q[0][0] > SLOT.0 as f32 / 2.0 && q[1][0] > SLOT.0 as f32 / 2.0, "{q:?}");
    }

    /// `id`'s body showing a `shown` frame turned `turn`, and the flat screen the view lays out
    /// for it in `slot` (centre offset and height, device pixels).
    fn shown(id: &str, shown: (u32, u32), turn: sim_core::rotation::Rotation, slot: (u32, u32)) -> (Job, [f32; 3]) {
        let body = sim_core::body::get(id).unwrap();
        let placement = super::super::place(&body, shown, turn).unwrap();
        let mesh = sim_core::body::cached_mesh(&body);
        let job = Job { back: facing_back(&body, &placement), body, mesh, placement, camera: Camera::default(), turn: Turn::default() };
        let aspect = shown.0 as f32 / shown.1 as f32;
        let [x, y, w, h] = super::super::screen_rect([0.0, 0.0, slot.0 as f32, slot.1 as f32], 2.0, [24.0, aspect, 0.0, 0.0]);
        (job, [x + w / 2.0 - slot.0 as f32 / 2.0, y + h / 2.0 - slot.1 as f32 / 2.0, h])
    }

    #[test]
    fn zoomed_in_and_orbited_the_eye_stays_outside_the_body() {
        use sim_core::rotation::Rotation;
        let fold = sim_core::body::get("pixel_fold").unwrap();
        let (inner, outer) =
            (fold.screen(sim_core::body::Display::Inner).unwrap().px, fold.screen(sim_core::body::Display::Outer).unwrap().px);
        // A landscape iPhone in a window refitted to it (the flat screen height-limited, 1404 of
        // 1500 px), and a foldable open and folded shut.
        let cases = [
            ("landscape iPhone", (2622, 1206), Rotation::Deg90, (3200, 1500)),
            ("open fold", inner, Rotation::Deg0, (1700, 1500)),
            ("folded fold", outer, Rotation::Deg0, (1700, 1500)),
        ];
        for (name, px, turn, slot) in cases {
            let id = if name == "landscape iPhone" { "iPhone-17" } else { "pixel_fold" };
            let (mut job, flat) = shown(id, px, turn, slot);
            let (center, half) = job.bounds(&job.root());
            let radius = (half[0].hypot(half[1])).hypot(half[2]);
            let dist = |eye: [f32; 3]| ((eye[0] - center[0]).hypot(eye[1] - center[1])).hypot(eye[2] - center[2]);
            // ⌘0, pinched in all the way, then orbited round and tilted, panned or not.
            for pan in [[0.0, 0.0], [0.5, -0.5]] {
                for pitch in [-1.2f32, -0.5, 0.0, 0.6, 1.39] {
                    let mut last: Option<f32> = None;
                    for step in -90..=90 {
                        let yaw = step as f32 * 2f32.to_radians();
                        job.camera = Camera { yaw, pitch, pan, ..Camera::preset(super::super::Preset::Face).zoom_by(2.0) };
                        let eye = job.solve(slot, flat).eye;
                        let d = dist(eye);
                        assert!(
                            d > radius * 1.01,
                            "{name}: the eye is {d} mm from the centre, inside the {radius} mm sphere ({:?})",
                            job.camera
                        );
                        // Smoothly: a 2° step moves the eye a little, never a jump.
                        if let Some(l) = last {
                            assert!((d - l).abs() < 0.06 * l, "{name}: {l} -> {d} at yaw {yaw}, pitch {pitch}");
                        }
                        last = Some(d);
                    }
                }
            }
            // Zooming in face on and orbited 100°: the eye comes in, then stops outside.
            let mut last = f32::MAX;
            for z in 0..=40 {
                job.camera = Camera { yaw: 100f32.to_radians(), ..Camera::preset(super::super::Preset::Face).zoom_by(z as f32 / 20.0) };
                let d = dist(job.solve(slot, flat).eye);
                assert!(d > radius * 1.01 && d <= last + 1e-3, "{name}: zoom {z}: {last} -> {d}");
                last = d;
            }
        }
    }

    #[test]
    fn face_on_is_one_to_one_for_every_screen_of_every_body_in_the_table() {
        // Every row's every screen showing a frame of its own pixels, face on, in a tall and a
        // wide slot: its corners are the flat screen's to a hundredth of a pixel. A row a form
        // factor's module adds is covered here as it lands.
        let mut checked = 0;
        for body in sim_core::body::all() {
            for s in &body.screens {
                for slot in [(1700, 1500), (3200, 1500)] {
                    let (mut job, flat) = shown(&body.id, s.px, sim_core::rotation::Rotation::Deg0, slot);
                    let what = format!("{} {:?} in {slot:?}", body.id, s.display);
                    assert_eq!(job.placement.display, s.display, "{what}: the frame goes on its screen");
                    job.camera = Camera::preset(super::super::Preset::Face);
                    let q = job.solve(slot, flat).screen.unwrap_or_else(|| panic!("{what}: face on, the screen faces the camera"));
                    let h = flat[2];
                    let w = h * s.px.0 as f32 / s.px.1 as f32;
                    let (x, y) = (slot.0 as f32 / 2.0 + flat[0] - w / 2.0, slot.1 as f32 / 2.0 + flat[1] - h / 2.0);
                    let want = [[x, y], [x + w, y], [x + w, y + h], [x, y + h]];
                    // Seen from the front or the back, each corner lands on one of the flat screen's.
                    let near = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 0.01 && (a[1] - b[1]).abs() < 0.01;
                    assert!(want.iter().all(|w| q.iter().any(|c| near(*c, *w))), "{what}: {q:?} vs {want:?}");
                    assert!(q.iter().all(|c| want.iter().any(|w| near(*c, *w))), "{what}: {q:?} vs {want:?}");
                    checked += 1;
                }
            }
        }
        assert!(checked >= 2 * sim_core::body::all().len(), "every row has a screen");
    }

    #[test]
    fn face_on_at_zoom_zero_is_still_one_to_one_on_a_landscape_phone() {
        // The pin beside the clamp: the eye is outside the body already, so 1:1 is untouched.
        let slot = (3200, 1500);
        let (mut job, flat) = shown("iPhone-17", (2622, 1206), sim_core::rotation::Rotation::Deg90, slot);
        job.camera = Camera::preset(super::super::Preset::Face);
        let q = job.solve(slot, flat).screen.expect("face on, the screen faces the camera");
        let h = (q[3][1] - q[0][1]).abs().max((q[1][1] - q[0][1]).abs());
        assert!((h - flat[2]).abs() < 0.05, "{q:?} vs {flat:?}");
    }
}
