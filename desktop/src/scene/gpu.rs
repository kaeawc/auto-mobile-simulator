//! The compositor: with the 3D view off it draws Rune's default frame (`Frame::draw_ui`), so the
//! window is what it is without one. With it on it draws the frame through
//! `Renderer::draw_with_backdrop`; at the scene's slot, while the pass is closed, it renders the
//! body into its own 4x MSAA colour and depth targets (Rune's passes have neither), sampling the
//! flat screen's rect of the backdrop copy for the live screen, then draws the resolved picture
//! into the slot. Nothing is cached between frames: the window draws only when something changed
//! (a device frame, input, a resize), so a still scene costs nothing.

use std::num::NonZeroU64;
use std::sync::Arc;

use rune::compose::{Compositor, DeviceKey, Frame};
use rune::render::{EffectPhase, EffectSite};
use sim_core::body::{Body, Face, Mesh};

use super::camera::{self, Camera, Mat4};
use super::layout::{self, STRIDE};
use super::{MARK, Placement, Preset};
use crate::State;

const SAMPLES: u32 = 4;
const COLOR: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const DEPTH: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
/// Bytes of the frame uniform (`Frame` in scene.wgsl) and of one draw's (`Draw`).
const FRAME_BYTES: u64 = 128;
const DRAW_BYTES: u64 = 96;
/// Towards the light in the camera's frame: from above, a little left and in front.
const LIGHT: [f32; 3] = [-0.35, 0.65, 0.7];
const AMBIENT: f32 = 0.28;

/// What one frame of the scene draws, read from the state before Rune's passes borrow it.
struct Job {
    body: Arc<Body>,
    mesh: Arc<Mesh>,
    placement: Placement,
    preset: Preset,
    /// The screen shown faces away from the front once the hinge is set (a book's cover screen
    /// folded shut, any back-face screen); a flip's cover screen on its lid faces the front shut.
    back: bool,
}

impl Job {
    fn from_state(s: &State) -> Option<Job> {
        let body = super::active(s)?;
        let placement = super::place(&body, s.screen_size(), s.screen_rotation())?;
        let mesh = sim_core::body::cached_mesh(&body);
        let back = facing_back(&body, &placement);
        Some(Job { body, mesh, placement, preset: s.view3d.preset, back })
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
        let points = mesh.parts.iter().flat_map(move |part| {
            let m = models[part.panel.min(1)];
            mesh.part_indices(part).iter().map(move |&i| camera::apply(&m, mesh.positions[i as usize]))
        });
        camera::bounds(points)
    }
}

/// Whether the screen `placement` shows faces -z with the hinge where it puts it: its face's
/// normal turned by its panel's transform.
fn facing_back(body: &Body, placement: &Placement) -> bool {
    let Some(screen) = body.screen(placement.display) else { return false };
    let z = if screen.face == Face::Front { 1.0 } else { -1.0 };
    let m = match &body.fold {
        Some(f) if body.panel_at(screen.center) == 1 => f.hinge.transform(placement.hinge),
        _ => return z < 0.0,
    };
    // The normal's z after the panel's turn: the third column's z.
    m[2][2] * z < 0.0
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
        f.ui.draw_with_backdrop(f.encoder, f.target, clear, &mut |phase, site| {
            if site.mark != MARK {
                return;
            }
            match phase {
                EffectPhase::Backdrop(encoder) => gpu.draw_scene(device, queue, encoder, site, &job),
                EffectPhase::Draw(pass) => gpu.blit(device, queue, pass, site),
            }
        });
        self.drew = gpu.ready;
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
        self.mesh = Some(MeshBuffers {
            id: job.body.id.clone(),
            vertices: buffer("simee body vertices", layout::vertices(&job.mesh), wgpu::BufferUsages::VERTEX),
            indices: buffer("simee body indices", layout::indices(&job.mesh), wgpu::BufferUsages::INDEX),
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
    /// `site.backdrop` (the frame up to the slot, the flat screen included).
    fn draw_scene(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        site: &EffectSite<'_>,
        job: &Job,
    ) {
        let Some(backdrop) = site.backdrop else { return };
        let r = site.instance.rect;
        let size = ((r[2].round() as u32).max(1), (r[3].round() as u32).max(1));
        self.targets(device, size);
        self.mesh(device, job);
        self.draws(device, job.mesh.parts.len());
        let (Some(t), Some(mesh), Some((_, draw_u, draw_group))) = (&self.targets, &self.mesh, &self.draws) else { return };

        let root = job.root();
        let camera = Camera::new(job.preset, job.back);
        let (view_proj, eye) = camera.view_proj(job.bounds(&root), size.0 as f32 / size.1 as f32);
        let light = camera.world(LIGHT);
        let bd = backdrop.texture();
        let (bw, bh) = (bd.width().max(1) as f32, bd.height().max(1) as f32);
        let [x, y, w, h] = super::screen_rect(r, site.scale, site.data);
        let (x, y) = (x - site.origin[0], y - site.origin[1]);
        let mut frame: Vec<f32> = flat(&view_proj).collect();
        frame.extend([eye[0], eye[1], eye[2], 1.0, light[0], light[1], light[2], AMBIENT]);
        frame.extend([x / bw, y / bh, w / bw, h / bh, if site.srgb_blend { 1.0 } else { 0.0 }, job.placement.quarters as f32, 0.0, 0.0]);
        queue.write_buffer(&self.frame_u, 0, &bytes(&frame));
        let mut draws = vec![0u8; job.mesh.parts.len() * self.draw_stride as usize];
        for (i, part) in job.mesh.parts.iter().enumerate() {
            let m = layout::material(part.role, job.placement.display);
            let mut d: Vec<f32> = flat(&job.panel(&root, part.panel)).collect();
            d.extend([m.base[0], m.base[1], m.base[2], if m.live { 1.0 } else { 0.0 }, m.specular, m.shininess, 0.0, 0.0]);
            let at = i * self.draw_stride as usize;
            draws[at..at + DRAW_BYTES as usize].copy_from_slice(&bytes(&d));
        }
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
        let bg = super::background();
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
        drop(pass);
        self.ready = true;
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
        // view_proj, eye, light, uv_rect, flags.
        assert_eq!(FRAME_BYTES, 64 + 4 * 16);
        // model, base, params.
        assert_eq!(DRAW_BYTES, 64 + 2 * 16);
        assert_eq!(bytes(&[1.0, -2.0]).len(), 8);
        assert_eq!(flat(&IDENTITY).count(), 16);
    }
}
