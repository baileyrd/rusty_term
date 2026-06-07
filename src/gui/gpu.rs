//! GPU renderer (wgpu): a glyph-atlas + instanced-quad compositor.
//!
//! Each distinct glyph is rasterized (via the shared [`FontCache`]) into a
//! cell-sized tile in an R8 coverage atlas. One instance per grid cell carries
//! its `(col, row, atlas slot, fg, bg)`; the vertex shader expands a cell quad,
//! the fragment shader mixes `bg`→`fg` by the sampled coverage. [`GpuCore`] is
//! target-agnostic (renders to any texture view), so the windowed
//! [`GpuRenderer`] and the headless render-to-texture test share it.

use std::collections::HashMap;
use std::sync::Arc;

use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::core::{Cell, CursorShape, Grid, WIDE_TRAILER, char_width};

use super::font::{FontCache, GlyphSource};
use super::render::Renderer;

/// Square slot grid in the atlas: up to `SLOTS_PER_ROW²` distinct glyphs.
const SLOTS_PER_ROW: u32 = 32;

const SHADER: &str = r#"
struct Uniforms { screen: vec2<f32>, cell: vec2<f32>, slots_per_row: u32 };
@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlas_tex: texture_2d<f32>;
@group(0) @binding(2) var atlas_smp: sampler;

struct Inst {
    @location(0) col: u32,
    @location(1) row: u32,
    @location(2) slot: u32,
    @location(3) fg: u32,
    @location(4) bg: u32,
    @location(5) curs: u32,
    @location(6) ccol: u32,
};
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) fg: vec4<f32>,
    @location(2) bg: vec4<f32>,
    @location(3) @interpolate(flat) curs: u32,
    @location(4) @interpolate(flat) ccol: vec4<f32>,
    @location(5) local: vec2<f32>,
};

fn unpack(c: u32) -> vec4<f32> {
    return vec4(f32((c >> 16u) & 0xffu) / 255.0,
                f32((c >> 8u) & 0xffu) / 255.0,
                f32(c & 0xffu) / 255.0, 1.0);
}

@vertex
fn vs(@builtin(vertex_index) vi: u32, inst: Inst) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
        vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0));
    let corner = corners[vi];
    let px = (vec2<f32>(f32(inst.col), f32(inst.row)) + corner) * u.cell;
    let ndc = vec2(px.x / u.screen.x * 2.0 - 1.0, 1.0 - px.y / u.screen.y * 2.0);
    let spr = f32(u.slots_per_row);
    let slot_col = f32(inst.slot % u.slots_per_row);
    let slot_row = f32(inst.slot / u.slots_per_row);
    var out: VsOut;
    out.pos = vec4(ndc, 0.0, 1.0);
    out.uv = (vec2(slot_col, slot_row) + corner) / spr;
    out.fg = unpack(inst.fg);
    out.bg = unpack(inst.bg);
    out.curs = inst.curs;
    out.ccol = unpack(inst.ccol);
    out.local = corner;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let a = textureSample(atlas_tex, atlas_smp, in.uv).r;
    let base = mix(in.bg, in.fg, a);
    // curs: 0 none/block (block uses the fg/bg swap); 2 underline, 3 bar.
    if (in.curs == 2u && in.local.y >= 0.85) { return in.ccol; }
    if (in.curs == 3u && in.local.x <= 0.12) { return in.ccol; }
    return base;
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    col: u32,
    row: u32,
    slot: u32,
    fg: u32,
    bg: u32,
    curs: u32,
    ccol: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    screen: [f32; 2],
    cell: [f32; 2],
    slots_per_row: u32,
    _pad: [u32; 3],
}

/// Target-agnostic GPU compositor: device, pipeline, and glyph atlas.
pub(crate) struct GpuCore {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    atlas: wgpu::Texture,
    uniform_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    cell_w: u32,
    cell_h: u32,
    slots: HashMap<char, u32>,
    next_slot: u32,
    /// The color format render targets must use.
    pub(crate) format: wgpu::TextureFormat,
}

impl GpuCore {
    /// Build the device, pipeline, and atlas. `compatible_surface` (when windowed)
    /// constrains adapter selection and the chosen format. `None` on no adapter.
    pub(crate) fn new(
        instance: &wgpu::Instance,
        compatible_surface: Option<&wgpu::Surface<'static>>,
        font: &mut FontCache,
    ) -> Option<GpuCore> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface,
            force_fallback_adapter: false,
        }))?;
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("rusty_term"),
                required_features: wgpu::Features::empty(),
                // Use the adapter's real capabilities, not `downlevel_defaults()`
                // (whose `max_texture_dimension_2d` is 2048) — a native window is
                // routinely wider than 2048px, and the surface is a texture, so a
                // low cap makes `Surface::configure` reject normal window sizes.
                required_limits: adapter.limits(),
                memory_hints: wgpu::MemoryHints::default(),
            },
            None,
        ))
        .ok()?;

        let format = match compatible_surface {
            Some(surface) => {
                let caps = surface.get_capabilities(&adapter);
                caps.formats
                    .iter()
                    .copied()
                    .find(|f| !f.is_srgb())
                    .unwrap_or(caps.formats[0])
            }
            None => wgpu::TextureFormat::Rgba8Unorm,
        };

        let (cell_w, cell_h) = font.cell_size();
        let (cell_w, cell_h) = (cell_w.max(1) as u32, cell_h.max(1) as u32);
        let atlas = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph atlas"),
            size: wgpu::Extent3d {
                width: SLOTS_PER_ROW * cell_w,
                height: SLOTS_PER_ROW * cell_h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let atlas_view = atlas.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor::default());

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bind layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
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
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bind group"),
            layout: &bind_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: uniform_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&atlas_view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cells"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pipeline layout"),
            bind_group_layouts: &[&bind_layout],
            push_constant_ranges: &[],
        });
        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Instance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![0 => Uint32, 1 => Uint32, 2 => Uint32, 3 => Uint32, 4 => Uint32, 5 => Uint32, 6 => Uint32],
        };
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cells pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs",
                buffers: &[instance_layout],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs",
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let mut core = GpuCore {
            device,
            queue,
            pipeline,
            atlas,
            uniform_buf,
            bind_group,
            cell_w,
            cell_h,
            slots: HashMap::new(),
            next_slot: 0,
            format,
        };
        // Reserve slot 0 for the blank tile (space / overflow fallback).
        core.ensure_slot(' ', font);
        Some(core)
    }

    /// Ensure `ch` has an atlas slot, rasterizing+uploading its cell tile on
    /// first use. Returns the slot (slot 0 if the atlas is full).
    fn ensure_slot(&mut self, ch: char, font: &mut FontCache) -> u32 {
        if let Some(&s) = self.slots.get(&ch) {
            return s;
        }
        if self.next_slot >= SLOTS_PER_ROW * SLOTS_PER_ROW {
            return 0;
        }
        let slot = self.next_slot;
        self.next_slot += 1;
        let tile = cell_tile(font, ch, self.cell_w as usize, self.cell_h as usize);
        let x = (slot % SLOTS_PER_ROW) * self.cell_w;
        let y = (slot / SLOTS_PER_ROW) * self.cell_h;
        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.atlas,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &tile,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(self.cell_w),
                rows_per_image: Some(self.cell_h),
            },
            wgpu::Extent3d { width: self.cell_w, height: self.cell_h, depth_or_array_layers: 1 },
        );
        self.slots.insert(ch, slot);
        slot
    }

    /// Render `grid` into `view` (a surface frame or an offscreen texture).
    /// A non-empty `chrome` row is drawn as cell row 0 with the grid shifted
    /// one row down (see [`Renderer::render`]).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render(
        &mut self,
        view: &wgpu::TextureView,
        width: u32,
        height: u32,
        grid: &Grid,
        chrome: &[Cell],
        font: &mut FontCache,
        cursor_on: bool,
    ) {
        let uniforms = Uniforms {
            screen: [width.max(1) as f32, height.max(1) as f32],
            cell: [self.cell_w as f32, self.cell_h as f32],
            slots_per_row: SLOTS_PER_ROW,
            _pad: [0; 3],
        };
        self.queue.write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));

        // The block cursor paints the cell in the cursor color (OSC 12 / the
        // `cursor` config key) with the glyph in the cell's bg; drag-selection
        // inverts the cell's own fg/bg (no shader change either way). The
        // cursor shows only on the live view, not while scrolled into history.
        let cursor = (grid.cursor_visible && grid.view_offset == 0 && cursor_on).then_some(grid.cursor);
        let status = grid.status_row();
        let last_row = grid.rows.saturating_sub(1);
        let row_off = if chrome.is_empty() { 0 } else { 1 };
        let mut instances = Vec::with_capacity(grid.cells.len() + chrome.len());
        for (col, cell) in chrome.iter().enumerate() {
            if cell.flags & WIDE_TRAILER != 0 {
                continue;
            }
            let slot = self.ensure_slot(cell.ch, font);
            instances.push(Instance { col: col as u32, row: 0, slot, fg: cell.fg, bg: cell.bg, curs: 0, ccol: 0 });
        }
        for i in 0..grid.cols * grid.rows {
            let (col, row) = (i % grid.cols, i / grid.cols);
            // The status-line overlay (L13), when present, replaces the bottom row;
            // otherwise `viewport_cell` composites scrollback history when scrolled.
            let on_status = status.is_some() && row == last_row;
            let cell = if on_status { status.unwrap()[col] } else { grid.viewport_cell(col, row) };
            if cell.flags & WIDE_TRAILER != 0 {
                continue;
            }
            // Selection coordinates address the live grid, so highlight only the
            // live view; history rows don't line up while scrolled.
            let (fg, bg, curs, ccol) = if !on_status && cursor == Some((col, row)) {
                match grid.cursor_shape {
                    CursorShape::Block => (cell.bg, grid.cursor_color, 0u32, 0u32),
                    CursorShape::Underline => (cell.fg, cell.bg, 2u32, grid.cursor_color),
                    CursorShape::Bar => (cell.fg, cell.bg, 3u32, grid.cursor_color),
                }
            } else if !on_status && grid.view_offset == 0 && grid.is_selected(col, row) {
                (cell.bg, cell.fg, 0, 0)
            } else {
                (cell.fg, cell.bg, 0, 0)
            };
            let slot = self.ensure_slot(cell.ch, font);
            instances.push(Instance {
                col: col as u32,
                row: (row + row_off) as u32,
                slot,
                fg,
                bg,
                curs,
                ccol,
            });
        }
        // IME preedit (composition): reverse-video glyphs at the cursor.
        if !grid.ime_preedit.is_empty() && grid.view_offset == 0 {
            let crow = grid.cursor.1;
            let mut col = grid.cursor.0;
            for pch in grid.ime_preedit.chars() {
                let w = char_width(pch).max(1);
                if col + w > grid.cols {
                    break;
                }
                let base = grid.viewport_cell(col, crow);
                let row = (crow + row_off) as u32;
                let slot = self.ensure_slot(pch, font);
                instances.push(Instance { col: col as u32, row, slot, fg: base.bg, bg: base.fg, curs: 0, ccol: 0 });
                if w == 2 {
                    let blank = self.ensure_slot(' ', font);
                    instances.push(Instance { col: col as u32 + 1, row, slot: blank, fg: base.bg, bg: base.fg, curs: 0, ccol: 0 });
                }
                col += w;
            }
        }
        let instance_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("instances"),
            contents: bytemuck::cast_slice(&instances),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("cells pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, instance_buf.slice(..));
            pass.draw(0..6, 0..instances.len() as u32);
        }
        self.queue.submit([encoder.finish()]);
    }
}

/// Build a `cell_w × cell_h` R8 coverage tile for `ch`, blitting the glyph at
/// its bearing within the cell box.
fn cell_tile(font: &mut FontCache, ch: char, cell_w: usize, cell_h: usize) -> Vec<u8> {
    let baseline = font.baseline();
    let glyph = font.glyph(ch);
    let mut tile = vec![0u8; cell_w * cell_h];
    for gy in 0..glyph.height {
        let ty = baseline + glyph.top + gy as i32;
        if ty < 0 || ty as usize >= cell_h {
            continue;
        }
        for gx in 0..glyph.width {
            let tx = glyph.left + gx as i32;
            if tx < 0 || tx as usize >= cell_w {
                continue;
            }
            tile[ty as usize * cell_w + tx as usize] = glyph.coverage[gy * glyph.width + gx];
        }
    }
    tile
}

/// The windowed GPU renderer: a `GpuCore` presenting to a `wgpu` surface.
pub(crate) struct GpuRenderer {
    core: GpuCore,
    surface: wgpu::Surface<'static>,
    configured: (u32, u32),
}

impl GpuRenderer {
    pub(crate) fn new(window: Arc<Window>, font: &mut FontCache) -> Result<Self, Box<dyn std::error::Error>> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let surface = instance.create_surface(window)?;
        let core = GpuCore::new(&instance, Some(&surface), font).ok_or("no GPU adapter")?;
        Ok(GpuRenderer { core, surface, configured: (0, 0) })
    }
}

impl Renderer for GpuRenderer {
    fn render(&mut self, grid: &Grid, chrome: &[Cell], font: &mut FontCache, width: u32, height: u32, cursor_on: bool) {
        if width == 0 || height == 0 {
            return;
        }
        // wgpu rejects a surface whose width or height exceeds the device's max
        // 2D texture size (the surface is texture-backed), so clamp and shadow.
        // A window dragged past that limit renders cropped instead of panicking.
        let max = self.core.device.limits().max_texture_dimension_2d;
        let width = width.min(max);
        let height = height.min(max);
        if self.configured != (width, height) {
            self.surface.configure(
                &self.core.device,
                &wgpu::SurfaceConfiguration {
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    format: self.core.format,
                    width,
                    height,
                    present_mode: wgpu::PresentMode::Fifo,
                    desired_maximum_frame_latency: 2,
                    alpha_mode: wgpu::CompositeAlphaMode::Auto,
                    view_formats: vec![],
                },
            );
            self.configured = (width, height);
        }
        let Ok(frame) = self.surface.get_current_texture() else {
            return;
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.core.render(&view, width, height, grid, chrome, font, cursor_on);
        frame.present();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::AnsiParser;

    /// The adapter, WGSL shader, render pipeline, and glyph atlas all build.
    /// Skips cleanly when no font or no wgpu adapter is available. Covers the
    /// bulk of the GPU path; the actual draw is exercised by the ignored
    /// `gpu_renders_to_texture`.
    #[test]
    fn gpu_core_builds() {
        let Some(bytes) = super::super::font::load_default_font(None) else {
            eprintln!("no system font; skipping GPU core test");
            return;
        };
        let mut font = FontCache::new(bytes, 16.0).unwrap();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let Some(core) = GpuCore::new(&instance, None, &mut font) else {
            eprintln!("no wgpu adapter; skipping GPU core test");
            return;
        };
        assert!(core.cell_w > 0 && core.cell_h > 0);
        assert_eq!(core.format, wgpu::TextureFormat::Rgba8Unorm);
    }

    /// Full render-to-texture + readback: a blank blue cell should read back as
    /// blue. Ignored by default because the software drivers in headless CI/WSL
    /// (lavapipe, dzn) segfault during draw/submit; run with `--ignored` on a
    /// machine with a working GPU/Vulkan adapter.
    #[test]
    #[ignore = "render+readback needs a working GPU adapter; lavapipe/dzn crash headless"]
    fn gpu_renders_to_texture() {
        let mut font = FontCache::new(super::super::font::load_default_font(None).unwrap(), 16.0).unwrap();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let mut core = GpuCore::new(&instance, None, &mut font).expect("adapter");

        let mut grid = Grid::new(1, 1);
        let mut p = AnsiParser::new();
        p.advance(&mut grid, b"\x1b[48;2;0;0;255m ");

        let (w, h) = (core.cell_w, core.cell_h);
        let target = core.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("target"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        core.render(&view, w, h, &grid, &[], &mut font, true);

        let bytes_per_row = (w * 4).next_multiple_of(256);
        let readback = core.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (bytes_per_row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = core
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        enc.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &readback,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        core.queue.submit([enc.finish()]);
        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        core.device.poll(wgpu::Maintain::Wait);
        let data = readback.slice(..).get_mapped_range();
        assert_eq!([data[0], data[1], data[2]], [0, 0, 255]);
    }
}
