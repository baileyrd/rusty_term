//! GPU renderer (wgpu): a shelf-packed RGBA glyph atlas + instanced quads,
//! in three layers sharing one render pass:
//!
//! 1. **Base cells** (opaque, `REPLACE` blending): one quad per cell (or per
//!    wide-glyph lead, spanning its cells) carrying background, cursor, and
//!    underline/strike decoration state; single-cell glyphs sample their
//!    atlas tile directly, mixed `bg`→`fg` by coverage in the shader.
//! 2. **Overlay glyphs** (alpha-blended): shaped ligature runs — the same
//!    GSUB plan the CPU renderer uses — drawn as multi-cell quads over the
//!    base layer, so a ligature can span cells with differing backgrounds.
//! 3. **Images** (alpha-blended, own pipeline + per-image textures): placed
//!    Sixel/Kitty/iTerm2 images at full pixel resolution, and Kitty Unicode
//!    placeholder cells sampling their slice of the placement grid. Animated
//!    images re-upload per frame through a bounded texture cache.
//!
//! The atlas is RGBA: ordinary glyphs upload as white×coverage (so the
//! shader's `fg × texel` tint works unchanged), color-emoji bitmap strikes
//! upload their real pixels with the instance's `fg` forced to white.
//! Tiles are shelf-packed at any multiple of the cell width, replacing the
//! old fixed 32×32 one-cell slot grid (and its 1024-glyph cap).
//! [`GpuCore`] is target-agnostic (renders to any texture view), so the
//! windowed [`GpuRenderer`] and the headless render-to-texture test share it.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::core::{
    ATTR_BOLD, ATTR_ITALIC, ATTR_STRIKE, ATTR_UNDERLINE, ATTR_UNDERLINE_COLOR, Cell, CursorShape,
    Grid, UnderlineStyle, WIDE_TRAILER, char_width,
};

use super::font::{FontCache, Glyph, GlyphSource, Style};
use super::render::Renderer;

/// Search-match highlight (matches [`super::cpu`]): amber match, orange active.
const SEARCH_BG: u32 = 0xFFD24A;
const SEARCH_CUR_BG: u32 = 0xFF7A1A;
const SEARCH_FG: u32 = 0x101010;

/// Atlas texture edge (clamped to the device limit at creation).
const ATLAS_MAX: u32 = 2048;
/// Bound on cached per-image GPU textures; overflow clears the cache (the
/// next frame re-uploads what it actually draws).
const IMG_CACHE_MAX: usize = 32;

const SHADER: &str = r#"
struct Uniforms { screen: vec2<f32>, cell: vec2<f32>, atlas: vec2<f32>, opacity: f32, _pad: f32 };
@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlas_tex: texture_2d<f32>;
@group(0) @binding(2) var atlas_smp: sampler;

struct Inst {
    @location(0) col: u32,
    @location(1) row: u32,
    @location(2) span: u32,
    @location(3) uv_xy: u32,
    @location(4) uv_wh: u32,
    @location(5) fg: u32,
    @location(6) bg: u32,
    @location(7) curs: u32,
    @location(8) ccol: u32,
    @location(9) deco: u32,
    @location(10) dcol: u32,
    @location(11) kind: u32,
};
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) fg: vec4<f32>,
    @location(2) bg: vec4<f32>,
    @location(3) @interpolate(flat) curs: u32,
    @location(4) @interpolate(flat) ccol: vec4<f32>,
    @location(5) local: vec2<f32>,
    @location(6) @interpolate(flat) deco: u32,
    @location(7) dcol: vec4<f32>,
    @location(8) @interpolate(flat) kind: u32,
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
    let span = f32(max(inst.span, 1u));
    let px = (vec2<f32>(f32(inst.col), f32(inst.row)) + corner * vec2(span, 1.0)) * u.cell;
    let ndc = vec2(px.x / u.screen.x * 2.0 - 1.0, 1.0 - px.y / u.screen.y * 2.0);
    let uv0 = vec2(f32(inst.uv_xy >> 16u), f32(inst.uv_xy & 0xffffu));
    let uvd = vec2(f32(inst.uv_wh >> 16u), f32(inst.uv_wh & 0xffffu));
    var out: VsOut;
    out.pos = vec4(ndc, 0.0, 1.0);
    out.uv = (uv0 + corner * uvd) / u.atlas;
    out.fg = unpack(inst.fg);
    out.bg = unpack(inst.bg);
    out.curs = inst.curs;
    out.ccol = unpack(inst.ccol);
    out.local = corner;
    out.deco = inst.deco;
    out.dcol = unpack(inst.dcol);
    out.kind = inst.kind;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let t = textureSample(atlas_tex, atlas_smp, in.uv);
    // Overlay glyphs (shaped ligature runs) alpha-blend over the base layer;
    // the pipeline's alpha blend preserves the target's alpha (= opacity).
    if (in.kind == 1u) {
        return vec4(in.fg.rgb * t.rgb, t.a);
    }
    // Solid overlay fill (cursor-trail ghosts): fg at curs/255 alpha.
    if (in.kind == 2u) {
        return vec4(in.fg.rgb, f32(in.curs) / 255.0);
    }
    let glyph = vec4(in.fg.rgb * t.rgb, 1.0);
    let base = mix(in.bg, glyph, t.a);
    // curs: 0 none/block (block uses the fg/bg swap); 2 underline, 3 bar.
    if (in.curs == 2u && in.local.y >= 0.85) { return vec4(in.ccol.rgb, u.opacity); }
    if (in.curs == 3u && in.local.x <= 0.12) { return vec4(in.ccol.rgb, u.opacity); }
    // deco: bit 3 (8) strikethrough; bits 0-2 underline style (0 = none, per
    // UnderlineStyle::pack_into's 1-5 encoding). Drawn as thin stripes near
    // the cell's bottom (underline) or vertical middle (strike), matching the
    // CPU rasterizer's `draw_underline`/`draw_strike`.
    if ((in.deco & 8u) != 0u && in.local.y >= 0.45 && in.local.y < 0.55) {
        return vec4(in.dcol.rgb, u.opacity);
    }
    let style = in.deco & 7u;
    if (style != 0u) {
        let thick = 0.08;
        let bottom = 1.0 - thick - 0.05;
        if (style == 1u && in.local.y >= bottom && in.local.y < bottom + thick) {
            return vec4(in.dcol.rgb, u.opacity);
        }
        if (style == 2u && ((in.local.y >= bottom && in.local.y < bottom + thick)
            || (in.local.y >= bottom - 2.0 * thick && in.local.y < bottom - thick))) {
            return vec4(in.dcol.rgb, u.opacity);
        }
        if (style == 3u) {
            let amp = 0.06;
            let y = bottom + sin(in.local.x * 6.283185) * amp;
            if (in.local.y >= y - thick * 0.5 && in.local.y < y + thick * 0.5) {
                return vec4(in.dcol.rgb, u.opacity);
            }
        }
        if (style == 4u && in.local.y >= bottom && in.local.y < bottom + thick
            && fract(in.local.x * 6.0) < 0.5) {
            return vec4(in.dcol.rgb, u.opacity);
        }
        if (style == 5u && in.local.y >= bottom && in.local.y < bottom + thick
            && fract(in.local.x * 3.0) < 0.6) {
            return vec4(in.dcol.rgb, u.opacity);
        }
    }
    return vec4(base.rgb, u.opacity);
}
"#;

const IMG_SHADER: &str = r#"
struct Uniforms { screen: vec2<f32>, cell: vec2<f32>, atlas: vec2<f32>, opacity: f32, _pad: f32 };
@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(2) var smp: sampler;
@group(1) @binding(0) var img: texture_2d<f32>;

struct Inst {
    @location(0) col: i32,
    @location(1) row: i32,
    @location(2) cols: u32,
    @location(3) rows: u32,
    @location(4) uv: vec4<f32>,
};
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32, inst: Inst) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
        vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0));
    let corner = corners[vi];
    let size = vec2(f32(inst.cols), f32(inst.rows));
    let px = (vec2(f32(inst.col), f32(inst.row)) + corner * size) * u.cell;
    let ndc = vec2(px.x / u.screen.x * 2.0 - 1.0, 1.0 - px.y / u.screen.y * 2.0);
    var out: VsOut;
    out.pos = vec4(ndc, 0.0, 1.0);
    out.uv = inst.uv.xy + corner * inst.uv.zw;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let t = textureSample(img, smp, in.uv);
    return vec4(t.rgb, t.a);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    col: u32,
    row: u32,
    /// Quad width in cells (wide glyphs and ligatures span several).
    span: u32,
    /// Atlas tile origin, `x << 16 | y` in texels.
    uv_xy: u32,
    /// Atlas tile size, `w << 16 | h` in texels.
    uv_wh: u32,
    fg: u32,
    bg: u32,
    curs: u32,
    ccol: u32,
    /// Bit 3: strikethrough. Bits 0-2: underline style, `0` none else
    /// `UnderlineStyle`'s 1-5 encoding (straight/double/curly/dotted/dashed).
    deco: u32,
    /// Underline/strike stripe color (`dcol`), used only when `deco != 0`.
    dcol: u32,
    /// 0 = base cell (opaque), 1 = overlay glyph (alpha-blended).
    kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ImgInstance {
    /// Cell position; signed — a scrolled image's top row can be negative.
    col: i32,
    row: i32,
    cols: u32,
    rows: u32,
    /// Normalized source rect `(x, y, w, h)` within the image texture.
    uv: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    screen: [f32; 2],
    cell: [f32; 2],
    atlas: [f32; 2],
    /// Window background opacity (`[window] opacity` config key), `0.0`-`1.0`.
    /// Written as every base fragment's alpha; only visible when the surface
    /// was configured for straight-alpha compositing (see
    /// `GpuCore::alpha_mode`).
    opacity: f32,
    _pad: f32,
}

/// A glyph's identity in the atlas: plain chars by `(char, style)`, shaped
/// (GSUB) glyphs by their cached `Rc` pointer — stable for the `FontCache`'s
/// lifetime, and the renderer is rebuilt whenever the font is.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum TileKey {
    Char(char, Style),
    Shaped(usize),
}

/// Target-agnostic GPU compositor: device, pipelines, and glyph atlas.
pub(crate) struct GpuCore {
    device: wgpu::Device,
    queue: wgpu::Queue,
    base_pipeline: wgpu::RenderPipeline,
    overlay_pipeline: wgpu::RenderPipeline,
    img_pipeline: wgpu::RenderPipeline,
    atlas: wgpu::Texture,
    atlas_w: u32,
    atlas_h: u32,
    /// Shelf packer state: the current shelf's fill x and top y. Shelves are
    /// one cell tall; tiles any multiple of the cell width.
    shelf_x: u32,
    shelf_y: u32,
    tiles: HashMap<TileKey, (u32, u32, u32, u32)>,
    uniform_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    img_bind_layout: wgpu::BindGroupLayout,
    /// Per-image textures for the image pipeline, keyed by content identity
    /// (`(kind, id, revision)`), bounded by [`IMG_CACHE_MAX`].
    img_cache: HashMap<(u8, u64, u64), Rc<wgpu::BindGroup>>,
    cell_w: u32,
    cell_h: u32,
    /// The color format render targets must use.
    pub(crate) format: wgpu::TextureFormat,
    /// The composite-alpha mode negotiated at surface creation:
    /// `PostMultiplied` (straight, unmultiplied alpha — what the fragment
    /// shader produces) when the surface/platform offers it, else `Opaque`.
    /// Deliberately never `PreMultiplied`: the shader doesn't premultiply,
    /// so picking that mode would composite wrong (a bright halo) rather
    /// than just not being transparent — `Opaque` degrades gracefully
    /// instead. `None` for the headless (no real surface) test path.
    pub(crate) alpha_mode: Option<wgpu::CompositeAlphaMode>,
    /// Window background opacity in effect (`[window] opacity`), `0.0`-`1.0`.
    opacity: f32,
}

impl GpuCore {
    /// Build the device, pipelines, and atlas. `compatible_surface` (when
    /// windowed) constrains adapter selection and the chosen format. `None`
    /// on no adapter.
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

        let (format, alpha_mode) = match compatible_surface {
            Some(surface) => {
                let caps = surface.get_capabilities(&adapter);
                let format = caps
                    .formats
                    .iter()
                    .copied()
                    .find(|f| !f.is_srgb())
                    .unwrap_or(caps.formats[0]);
                let alpha_mode = caps
                    .alpha_modes
                    .contains(&wgpu::CompositeAlphaMode::PostMultiplied)
                    .then_some(wgpu::CompositeAlphaMode::PostMultiplied);
                (format, alpha_mode)
            }
            None => (wgpu::TextureFormat::Rgba8Unorm, None),
        };

        let (cell_w, cell_h) = font.cell_size();
        let (cell_w, cell_h) = (cell_w.max(1) as u32, cell_h.max(1) as u32);
        let max_dim = device.limits().max_texture_dimension_2d;
        let atlas_w = ATLAS_MAX.min(max_dim);
        let atlas_h = ATLAS_MAX.min(max_dim);
        let atlas = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph atlas"),
            size: wgpu::Extent3d { width: atlas_w, height: atlas_h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
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
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
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
        let img_bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image bind layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cells"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let img_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("images"),
            source: wgpu::ShaderSource::Wgsl(IMG_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pipeline layout"),
            bind_group_layouts: &[&bind_layout],
            push_constant_ranges: &[],
        });
        let img_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("image pipeline layout"),
            bind_group_layouts: &[&bind_layout, &img_bind_layout],
            push_constant_ranges: &[],
        });
        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Instance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![0 => Uint32, 1 => Uint32, 2 => Uint32, 3 => Uint32, 4 => Uint32, 5 => Uint32, 6 => Uint32, 7 => Uint32, 8 => Uint32, 9 => Uint32, 10 => Uint32, 11 => Uint32],
        };
        // Straight-alpha "over" for glyph overlays and images; the target's
        // alpha channel (the window opacity) is preserved, not blended away.
        let over = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::SrcAlpha,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Zero,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let make_cells_pipeline = |label: &str, blend: Option<wgpu::BlendState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: "vs",
                    buffers: std::slice::from_ref(&instance_layout),
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: "fs",
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            })
        };
        let base_pipeline = make_cells_pipeline("base cells", Some(wgpu::BlendState::REPLACE));
        let overlay_pipeline = make_cells_pipeline("overlay glyphs", Some(over));
        let img_instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<ImgInstance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![0 => Sint32, 1 => Sint32, 2 => Uint32, 3 => Uint32, 4 => Float32x4],
        };
        let img_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("images pipeline"),
            layout: Some(&img_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &img_shader,
                entry_point: "vs",
                buffers: &[img_instance_layout],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &img_shader,
                entry_point: "fs",
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(over),
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
            base_pipeline,
            overlay_pipeline,
            img_pipeline,
            atlas,
            atlas_w,
            atlas_h,
            shelf_x: 0,
            shelf_y: 0,
            tiles: HashMap::new(),
            uniform_buf,
            bind_group,
            img_bind_layout,
            img_cache: HashMap::new(),
            cell_w,
            cell_h,
            format,
            alpha_mode,
            opacity: 1.0,
        };
        // Reserve the first tile for blank (space / overflow fallback).
        core.tile_for_char(' ', Style::Regular, font);
        Some(core)
    }

    /// Shelf-allocate a `w × cell_h` texel rect, or `None` when the atlas is
    /// full (the caller recycles the atlas and retries).
    fn alloc_shelf(&mut self, w: u32) -> Option<(u32, u32)> {
        let w = w.min(self.atlas_w);
        if self.shelf_x + w > self.atlas_w {
            self.shelf_x = 0;
            self.shelf_y += self.cell_h;
        }
        if self.shelf_y + self.cell_h > self.atlas_h {
            return None;
        }
        let at = (self.shelf_x, self.shelf_y);
        self.shelf_x += w;
        Some(at)
    }

    /// Upload `glyph` as an RGBA tile spanning `span` cells and return its
    /// atlas rect. Ordinary coverage glyphs upload white×alpha (the shader
    /// tints by `fg`); color glyphs (emoji strikes) upload their own pixels.
    fn upload_tile(&mut self, key: TileKey, glyph: &Glyph, span: usize, baseline: i32) -> (u32, u32, u32, u32) {
        if let Some(&r) = self.tiles.get(&key) {
            return r;
        }
        let w = (span.max(1) as u32) * self.cell_w;
        let h = self.cell_h;
        let (x, y) = match self.alloc_shelf(w) {
            Some(at) => at,
            None => {
                // Atlas full: recycle it (drop every cached tile and restart the
                // shelf cursor) rather than rendering blanks forever. In-flight
                // frames still sample the old texels; the visual glitch lasts one
                // frame, versus permanently blank glyphs.
                self.tiles.clear();
                self.shelf_x = 0;
                self.shelf_y = 0;
                match self.alloc_shelf(w) {
                    Some(at) => at,
                    // Wider than the atlas itself: draw as background.
                    None => return (0, 0, 0, 0),
                }
            }
        };
        let mut tile = vec![0u8; (w * h) as usize * 4];
        for gy in 0..glyph.height {
            let ty = baseline + glyph.top + gy as i32;
            if ty < 0 || ty as u32 >= h {
                continue;
            }
            for gx in 0..glyph.width {
                let tx = glyph.left + gx as i32;
                if tx < 0 || tx as u32 >= w {
                    continue;
                }
                let o = ((ty as u32 * w) + tx as u32) as usize * 4;
                match &glyph.color {
                    Some(color) => {
                        let argb = color[gy * glyph.width + gx];
                        tile[o] = ((argb >> 16) & 0xFF) as u8;
                        tile[o + 1] = ((argb >> 8) & 0xFF) as u8;
                        tile[o + 2] = (argb & 0xFF) as u8;
                        tile[o + 3] = ((argb >> 24) & 0xFF) as u8;
                    }
                    None => {
                        let a = glyph.coverage[gy * glyph.width + gx];
                        tile[o] = 255;
                        tile[o + 1] = 255;
                        tile[o + 2] = 255;
                        tile[o + 3] = a;
                    }
                }
            }
        }
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
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        let rect = (x, y, w, h);
        self.tiles.insert(key, rect);
        rect
    }

    /// The atlas rect for a plain character (wide chars get a two-cell tile,
    /// so CJK/emoji are no longer clipped to their lead cell).
    fn tile_for_char(&mut self, ch: char, style: Style, font: &mut FontCache) -> ((u32, u32, u32, u32), bool) {
        let span = char_width(ch).max(1);
        let baseline = font.baseline();
        let glyph = font.glyph(ch, style);
        let color = glyph.color.is_some();
        (self.upload_tile(TileKey::Char(ch, style), &glyph, span, baseline), color)
    }

    /// Set the window background opacity (`[window] opacity`), clamped to
    /// `0.0..=1.0`. A no-op visually unless `alpha_mode` negotiated straight
    /// alpha compositing — see [`Self::alpha_mode`].
    pub(crate) fn set_opacity(&mut self, opacity: f32) {
        self.opacity = opacity.clamp(0.0, 1.0);
    }

    fn write_uniforms(&self, width: u32, height: u32) {
        let uniforms = Uniforms {
            screen: [width.max(1) as f32, height.max(1) as f32],
            cell: [self.cell_w as f32, self.cell_h as f32],
            atlas: [self.atlas_w as f32, self.atlas_h as f32],
            opacity: self.opacity,
            _pad: 0.0,
        };
        self.queue.write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));
    }

    /// A base (opaque) instance sampling `rect`.
    #[allow(clippy::too_many_arguments)]
    fn base_inst(col: u32, row: u32, span: u32, rect: (u32, u32, u32, u32), fg: u32, bg: u32, curs: u32, ccol: u32, deco: u32, dcol: u32) -> Instance {
        Instance {
            col,
            row,
            span,
            uv_xy: (rect.0 << 16) | (rect.1 & 0xFFFF),
            uv_wh: (rect.2 << 16) | (rect.3 & 0xFFFF),
            fg,
            bg,
            curs,
            ccol,
            deco,
            dcol,
            kind: 0,
        }
    }

    /// Render `grid` into `view` (a surface frame or an offscreen texture).
    /// A non-empty `chrome` row is drawn as cell row 0 with the grid shifted
    /// one row down (see [`Renderer::render`]).
    #[allow(clippy::too_many_arguments)]
    #[cfg(test)]
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
        self.write_uniforms(width, height);
        let row_off = if chrome.is_empty() { 0 } else { 1 };
        let mut frame = FrameLists::default();
        self.append_chrome(&mut frame, chrome, font);
        self.append_grid(&mut frame, grid, 0, row_off, true, cursor_on, font);
        self.draw_frame(view, &frame, 0x000000);
    }

    fn append_chrome(&mut self, frame: &mut FrameLists, chrome: &[Cell], font: &mut FontCache) {
        for (col, cell) in chrome.iter().enumerate() {
            if cell.flags & WIDE_TRAILER != 0 {
                continue;
            }
            let (rect, color) = self.tile_for_char(cell.ch, Style::Regular, font);
            let fg = if color { 0xFFFFFF } else { cell.fg };
            frame.base.push(Self::base_inst(col as u32, 0, char_width(cell.ch).max(1) as u32, rect, fg, cell.bg, 0, 0, 0, 0));
        }
    }

    /// Append `grid`'s cell, ligature-run, image, and IME-preedit draws at
    /// cell offset `(col0, row0)`. The cursor and preedit show only when
    /// `focused`; selection and search highlights come from the grid's state.
    #[allow(clippy::too_many_arguments)]
    fn append_grid(
        &mut self,
        frame: &mut FrameLists,
        grid: &Grid,
        col0: usize,
        row0: usize,
        focused: bool,
        cursor_on: bool,
        font: &mut FontCache,
    ) {
        let cursor = (focused && grid.cursor_visible && grid.view_offset == 0 && cursor_on)
            .then_some(grid.cursor);
        let status = grid.status_row();
        let last_row = grid.rows.saturating_sub(1);
        let blank = *self.tiles.get(&TileKey::Char(' ', Style::Regular)).unwrap_or(&(0, 0, 0, 0));

        // One resolved record per column of the current row, for run planning.
        struct Resolved {
            cell: Cell,
            fg: u32,
            bg: u32,
            curs: u32,
            ccol: u32,
            deco: u32,
            dcol: u32,
            /// Eligible to join a shaped (GSUB) run.
            run_ok: bool,
            /// Skip entirely (wide trailer).
            trailer: bool,
        }

        for row in 0..grid.rows {
            let on_status = status.is_some() && row == last_row;
            // Bidi (implicit mode): the row's visual/logical permutation, or
            // `None` (identity) for pure-LTR/status rows — mirrors the CPU
            // renderer. Cell state stays logical; only the emitted instance
            // X position (and glyph mirroring) go through the map.
            let bidi = if on_status { None } else { grid.bidi_row(row) };
            let vis = |col: usize| -> usize {
                bidi.as_ref().and_then(|b| b.log2vis.get(col)).map_or(col, |&v| v as usize)
            };
            let mut cols: Vec<Resolved> = Vec::with_capacity(grid.cols);
            for col in 0..grid.cols {
                let cell = if on_status { status.unwrap()[col] } else { grid.viewport_cell(col, row) };
                if cell.flags & WIDE_TRAILER != 0 {
                    cols.push(Resolved { cell, fg: 0, bg: 0, curs: 0, ccol: 0, deco: 0, dcol: 0, run_ok: false, trailer: true });
                    continue;
                }
                let (fg, bg, curs, ccol, plain) = if !on_status && cursor == Some((col, row)) {
                    let (fg, bg, curs, ccol) = match grid.cursor_shape {
                        CursorShape::Block => (cell.bg, grid.cursor_color, 0u32, 0u32),
                        CursorShape::Underline => (cell.fg, cell.bg, 2u32, grid.cursor_color),
                        CursorShape::Bar => (cell.fg, cell.bg, 3u32, grid.cursor_color),
                    };
                    (fg, bg, curs, ccol, false)
                } else if !on_status
                    && let Some(cur) = grid.search_highlight(col, row)
                {
                    (SEARCH_FG, if cur { SEARCH_CUR_BG } else { SEARCH_BG }, 0, 0, false)
                } else if !on_status && grid.is_selected(col, row) {
                    (cell.bg, cell.fg, 0, 0, false)
                } else {
                    (cell.fg, cell.bg, 0, 0, true)
                };
                // Minimum-contrast enforcement (`minimum_contrast` config),
                // mirroring the CPU renderer's glyph pass.
                let fg = crate::core::ensure_contrast(fg, bg, grid.min_contrast);
                let (deco, dcol) = deco_for(cell.flags, cell.underline_color, fg, plain);
                // Same run eligibility as the CPU renderer's ligature plan.
                let run_ok = plain
                    && cell.cluster == 0
                    && cell.ch != ' '
                    && char_width(cell.ch) == 1
                    && !super::boxdraw::is_synthesized(cell.ch)
                    && cell.ch != '\u{10EEEE}'
                    // A reordered (bidi) row draws cell-by-cell; a shaped run
                    // across visually non-adjacent cells would garble it.
                    && bidi.is_none()
                    && font.has_ligatures();
                cols.push(Resolved { cell, fg, bg, curs, ccol, deco, dcol, run_ok, trailer: false });
            }

            // Emit: shaped runs get blank-tile base cells + overlay glyph
            // quads; everything else keeps the single-instance direct path.
            let mut col = 0;
            while col < grid.cols {
                let r = &cols[col];
                if r.trailer {
                    col += 1;
                    continue;
                }
                if !r.run_ok {
                    let style = Style::new(r.cell.flags & ATTR_BOLD != 0, r.cell.flags & ATTR_ITALIC != 0);
                    let ch = if r.cell.ch == '\u{10EEEE}' { ' ' } else { r.cell.ch };
                    // Arabic contextual form first (phase 3), then rule L4
                    // mirroring for RTL-run chars.
                    let ch = match &bidi {
                        Some(b) => {
                            let ch = b.shaped.as_ref().and_then(|s| s[col]).unwrap_or(ch);
                            if b.rtl[col] {
                                crate::core::bidi_mirrored(ch).unwrap_or(ch)
                            } else {
                                ch
                            }
                        }
                        None => ch,
                    };
                    let (rect, is_color) = self.tile_for_char(ch, style, font);
                    let span = char_width(r.cell.ch).max(1) as u32;
                    let fg = if is_color { 0xFFFFFF } else { r.fg };
                    frame.base.push(Self::base_inst(
                        (col0 + vis(col)) as u32,
                        (row0 + row) as u32,
                        span,
                        rect,
                        fg,
                        r.bg,
                        r.curs,
                        r.ccol,
                        r.deco,
                        r.dcol,
                    ));
                    col += span as usize;
                    continue;
                }
                // A run: same style and fg, contiguous eligible cells.
                let style = Style::new(r.cell.flags & ATTR_BOLD != 0, r.cell.flags & ATTR_ITALIC != 0);
                let fg = r.fg;
                let start = col;
                let mut run: Vec<char> = Vec::new();
                while col < grid.cols {
                    let c = &cols[col];
                    let cstyle = Style::new(c.cell.flags & ATTR_BOLD != 0, c.cell.flags & ATTR_ITALIC != 0);
                    if !c.run_ok || cstyle != style || c.fg != fg {
                        break;
                    }
                    run.push(c.cell.ch);
                    col += 1;
                }
                // Base cells (background + decorations) under the run.
                for (i, c) in cols[start..col].iter().enumerate() {
                    frame.base.push(Self::base_inst(
                        (col0 + start + i) as u32,
                        (row0 + row) as u32,
                        1,
                        blank,
                        c.fg,
                        c.bg,
                        c.curs,
                        c.ccol,
                        c.deco,
                        c.dcol,
                    ));
                }
                // Shaped glyph overlays.
                let baseline = font.baseline();
                let mut pos = start;
                for (glyph, span) in font.shape(&run, style) {
                    if glyph.width != 0 {
                        let key = TileKey::Shaped(Rc::as_ptr(&glyph) as usize);
                        let rect = self.upload_tile(key, &glyph, span, baseline);
                        let fg = if glyph.color.is_some() { 0xFFFFFF } else { fg };
                        let mut inst = Self::base_inst(
                            (col0 + pos) as u32,
                            (row0 + row) as u32,
                            span as u32,
                            rect,
                            fg,
                            0,
                            0,
                            0,
                            0,
                            0,
                        );
                        inst.kind = 1;
                        frame.overlay.push(inst);
                    }
                    pos += span;
                }
            }
        }

        // Scrollbar overlay: cell-resolution thumb in the rightmost column.
        if let Some((first, len, color)) = grid.scrollbar() {
            for r in first..(first + len).min(grid.rows) {
                frame.base.push(Self::base_inst(
                    (col0 + grid.cols.saturating_sub(1)) as u32,
                    (row0 + r) as u32,
                    1,
                    blank,
                    color,
                    color,
                    0,
                    0,
                    0,
                    0,
                ));
            }
        }

        // Placed pixel images (Sixel/Kitty/iTerm2), full resolution — GPU
        // parity for the CPU renderer's overlay compositor.
        for im in grid.images() {
            if im.pw == 0 || im.ph == 0 || im.cols == 0 || im.rows == 0 {
                continue;
            }
            // An animated image (inline GIF) uploads its backing animation's
            // current frame, keyed by frame index so each frame gets its own
            // cached texture; the stored snapshot is the eviction fallback.
            let anim = im.anim.and_then(|id| {
                let (w, h, px) = grid.kitty_frame(id)?;
                (w == im.pw && h == im.ph).then_some(())?;
                let cur = grid.kitty_images.iter().find(|i| i.id == id)?.current;
                Some((id, cur, px))
            });
            let (key, pixels): (_, &[Option<u32>]) = match anim {
                Some((id, cur, px)) => ((3u8, id as u64, cur as u64), px),
                None => {
                    ((1u8, im.serial as u64, ((im.pw as u64) << 32) | im.ph as u64), &im.pixels)
                }
            };
            if let Some(bind) = self.image_texture(key, im.pw, im.ph, pixels) {
                frame.images.push((
                    bind,
                    ImgInstance {
                        col: (col0 + im.col) as i32,
                        row: row0 as i32 + grid.image_top_row(im) as i32,
                        cols: im.cols as u32,
                        rows: im.rows as u32,
                        uv: [0.0, 0.0, 1.0, 1.0],
                    },
                ));
            }
        }

        // Kitty Unicode placeholders: each cell samples its slice of the
        // placement grid from the image's current animation frame.
        if let Some(ph) = grid.placeholder_map() {
            for (i, entry) in ph.iter().enumerate() {
                let Some((id, prow, pcol)) = *entry else { continue };
                let Some((iw, ih, pixels)) = grid.kitty_frame(id) else { continue };
                let Some((pcols, prows)) =
                    grid.placeholder_grid(id, self.cell_w as usize, self.cell_h as usize)
                else {
                    continue;
                };
                let (prow, pcol) = (prow as usize, pcol as usize);
                if prow >= prows || pcol >= pcols {
                    continue;
                }
                let frame_idx = grid
                    .kitty_images
                    .iter()
                    .find(|im| im.id == id)
                    .map(|im| im.current as u64)
                    .unwrap_or(0);
                let key = (2u8, id as u64, frame_idx);
                let pixels = pixels.to_vec();
                if let Some(bind) = self.image_texture(key, iw, ih, &pixels) {
                    let (col, row) = (i % grid.cols, i / grid.cols);
                    frame.images.push((
                        bind,
                        ImgInstance {
                            col: (col0 + col) as i32,
                            row: (row0 + row) as i32,
                            cols: 1,
                            rows: 1,
                            uv: [
                                pcol as f32 / pcols as f32,
                                prow as f32 / prows as f32,
                                1.0 / pcols as f32,
                                1.0 / prows as f32,
                            ],
                        },
                    ));
                }
            }
        }

        if focused && !grid.ime_preedit.is_empty() && grid.view_offset == 0 {
            let crow = grid.cursor.1;
            let mut col = grid.cursor.0;
            for pch in grid.ime_preedit.chars() {
                let w = char_width(pch).max(1);
                if col + w > grid.cols {
                    break;
                }
                let base = grid.viewport_cell(col, crow);
                let (rect, is_color) = self.tile_for_char(pch, Style::Regular, font);
                let fg = if is_color { 0xFFFFFF } else { base.bg };
                frame.base.push(Self::base_inst(
                    (col0 + col) as u32,
                    (row0 + crow) as u32,
                    w as u32,
                    rect,
                    fg,
                    base.fg,
                    0,
                    0,
                    0,
                    0,
                ));
                col += w;
            }
        }
    }

    /// The cached (or freshly uploaded) texture bind group for image content
    /// `key`; `None` if the pixel buffer is inconsistent.
    fn image_texture(
        &mut self,
        key: (u8, u64, u64),
        w: usize,
        h: usize,
        pixels: &[Option<u32>],
    ) -> Option<Rc<wgpu::BindGroup>> {
        if pixels.len() < w * h {
            return None;
        }
        if let Some(b) = self.img_cache.get(&key) {
            return Some(Rc::clone(b));
        }
        if self.img_cache.len() >= IMG_CACHE_MAX {
            self.img_cache.clear();
        }
        let mut rgba = vec![0u8; w * h * 4];
        for (i, px) in pixels[..w * h].iter().enumerate() {
            if let Some(c) = px {
                rgba[i * 4] = ((c >> 16) & 0xFF) as u8;
                rgba[i * 4 + 1] = ((c >> 8) & 0xFF) as u8;
                rgba[i * 4 + 2] = (c & 0xFF) as u8;
                rgba[i * 4 + 3] = 255;
            }
        }
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("image"),
            size: wgpu::Extent3d { width: w as u32, height: h as u32, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &rgba,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(w as u32 * 4),
                rows_per_image: Some(h as u32),
            },
            wgpu::Extent3d { width: w as u32, height: h as u32, depth_or_array_layers: 1 },
        );
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = Rc::new(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image bind"),
            layout: &self.img_bind_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            }],
        }));
        self.img_cache.insert(key, Rc::clone(&bind));
        Some(bind)
    }

    /// Render a tab's `panes` into `view`, filling the gaps with `divider`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_panes(
        &mut self,
        view: &wgpu::TextureView,
        width: u32,
        height: u32,
        panes: &[super::render::PaneFrame],
        chrome: &[Cell],
        font: &mut FontCache,
        divider: u32,
    ) {
        self.write_uniforms(width, height);
        let mut frame = FrameLists::default();
        self.append_chrome(&mut frame, chrome, font);
        for p in panes {
            self.append_grid(&mut frame, p.grid, p.col0, p.row0, p.focused, p.cursor_on, font);
            // Cursor-trail ghosts (G36): solid cursor-colored fills on the
            // blended overlay layer, mirroring the CPU renderer's draw_trail.
            for &(col, row, alpha) in &p.trail {
                if col >= p.grid.cols || row >= p.grid.rows {
                    continue;
                }
                frame.overlay.push(Instance {
                    col: (p.col0 + col) as u32,
                    row: (p.row0 + row) as u32,
                    span: 1,
                    uv_xy: 0,
                    uv_wh: 0,
                    fg: p.grid.cursor_color,
                    bg: 0,
                    curs: (alpha.clamp(0.0, 1.0) * 255.0) as u32,
                    ccol: 0,
                    deco: 0,
                    dcol: 0,
                    kind: 2,
                });
            }
        }
        self.draw_frame(view, &frame, divider);
    }

    /// Submit the frame: base cells (opaque), overlay glyphs (blended), then
    /// image quads (blended, grouped by texture), clearing to `clear`.
    fn draw_frame(&self, view: &wgpu::TextureView, frame: &FrameLists, clear: u32) {
        let base_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("base instances"),
            contents: bytemuck::cast_slice(&frame.base),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let overlay_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("overlay instances"),
            contents: bytemuck::cast_slice(&frame.overlay),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let img_insts: Vec<ImgInstance> = frame.images.iter().map(|(_, i)| *i).collect();
        let img_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("image instances"),
            contents: bytemuck::cast_slice(&img_insts),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let c = wgpu::Color {
            r: ((clear >> 16) & 0xff) as f64 / 255.0,
            g: ((clear >> 8) & 0xff) as f64 / 255.0,
            b: (clear & 0xff) as f64 / 255.0,
            a: self.opacity as f64,
        };
        let mut encoder =
            self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("cells pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(c), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.base_pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, base_buf.slice(..));
            pass.draw(0..6, 0..frame.base.len() as u32);
            if !frame.overlay.is_empty() {
                pass.set_pipeline(&self.overlay_pipeline);
                pass.set_vertex_buffer(0, overlay_buf.slice(..));
                pass.draw(0..6, 0..frame.overlay.len() as u32);
            }
            if !frame.images.is_empty() {
                pass.set_pipeline(&self.img_pipeline);
                pass.set_vertex_buffer(0, img_buf.slice(..));
                for (i, (bind, _)) in frame.images.iter().enumerate() {
                    pass.set_bind_group(1, bind.as_ref(), &[]);
                    pass.draw(0..6, i as u32..i as u32 + 1);
                }
            }
        }
        self.queue.submit([encoder.finish()]);
    }
}

/// The per-frame draw lists the three layers accumulate into.
#[derive(Default)]
struct FrameLists {
    base: Vec<Instance>,
    overlay: Vec<Instance>,
    images: Vec<(Rc<wgpu::BindGroup>, ImgInstance)>,
}

/// Pack a cell's underline/strikethrough state into the `Instance::deco` bits
/// and pick the stripe color: `underline_color` when SGR 58 set it and `plain`
/// (the cell isn't under the cursor, a selection, or a search highlight —
/// those already swapped `fg` to something that should win instead).
fn deco_for(flags: u16, underline_color: u32, fg: u32, plain: bool) -> (u32, u32) {
    let mut deco = 0u32;
    if flags & ATTR_STRIKE != 0 {
        deco |= 8;
    }
    if flags & ATTR_UNDERLINE != 0 {
        deco |= match UnderlineStyle::from_attrs(flags) {
            UnderlineStyle::Straight => 1,
            UnderlineStyle::Double => 2,
            UnderlineStyle::Curly => 3,
            UnderlineStyle::Dotted => 4,
            UnderlineStyle::Dashed => 5,
        };
    }
    let dcol = if plain && flags & ATTR_UNDERLINE_COLOR != 0 { underline_color } else { fg };
    (deco, dcol)
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
    fn render(
        &mut self,
        panes: &[super::render::PaneFrame],
        chrome: &[Cell],
        font: &mut FontCache,
        width: u32,
        height: u32,
        divider: u32,
    ) {
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
                    alpha_mode: self.core.alpha_mode.unwrap_or(wgpu::CompositeAlphaMode::Auto),
                    view_formats: vec![],
                },
            );
            self.configured = (width, height);
        }
        let Ok(frame) = self.surface.get_current_texture() else {
            return;
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.core.render_panes(&view, width, height, panes, chrome, font, divider);
        frame.present();
    }

    fn set_opacity(&mut self, opacity: f32) {
        self.core.set_opacity(opacity);
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
        let mut font = FontCache::new(super::super::font::FontSet { regular: bytes, ..Default::default() }, 16.0, false).unwrap();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let Some(core) = GpuCore::new(&instance, None, &mut font) else {
            eprintln!("no wgpu adapter; skipping GPU core test");
            return;
        };
        assert!(core.cell_w > 0 && core.cell_h > 0);
        assert_eq!(core.format, wgpu::TextureFormat::Rgba8Unorm);
        // No compatible_surface (headless/test path): nothing to negotiate
        // alpha compositing with.
        assert_eq!(core.alpha_mode, None);
    }

    /// Builds a headless core, or `None` (with a note) when the environment
    /// has no usable font or wgpu adapter — tests skip cleanly in that case.
    fn headless_core() -> Option<(GpuCore, FontCache)> {
        let Some(bytes) = super::super::font::load_default_font(None) else {
            eprintln!("no system font; skipping GPU core test");
            return None;
        };
        let mut font = FontCache::new(super::super::font::FontSet { regular: bytes, ..Default::default() }, 16.0, false).unwrap();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let Some(core) = GpuCore::new(&instance, None, &mut font) else {
            eprintln!("no wgpu adapter; skipping GPU core test");
            return None;
        };
        Some((core, font))
    }

    /// The shelf allocator packs left-to-right, wraps to a new shelf at the
    /// row edge, and reports exhaustion (`None`) instead of overflowing.
    #[test]
    fn shelf_allocator_packs_wraps_and_exhausts() {
        let Some((mut core, _font)) = headless_core() else { return };
        // The blank tile is pre-reserved at the atlas origin.
        let blank = core.tiles[&TileKey::Char(' ', Style::Regular)];
        assert_eq!((blank.0, blank.1), (0, 0));
        assert_eq!((blank.2, blank.3), (core.cell_w, core.cell_h));

        // Sequential allocations advance along the shelf...
        let (x1, y1) = core.alloc_shelf(core.cell_w).unwrap();
        let (x2, y2) = core.alloc_shelf(core.cell_w).unwrap();
        assert_eq!(y1, y2);
        assert_eq!(x2, x1 + core.cell_w);

        // ...an allocation that can't fit wraps to the next shelf...
        let (x3, y3) = core.alloc_shelf(core.atlas_w).unwrap();
        assert_eq!((x3, y3), (0, y1 + core.cell_h));

        // ...and a full atlas yields None rather than clipping.
        core.shelf_y = core.atlas_h;
        assert_eq!(core.alloc_shelf(core.cell_w), None);
        // upload_tile then falls back to the pre-reserved blank rect.
        let g = Glyph { width: 0, height: 0, left: 0, top: 0, coverage: vec![], color: None };
        assert_eq!(core.upload_tile(TileKey::Char('q', Style::Bold), &g, 1, 0), blank);
    }

    /// Wide (two-column) characters get a two-cell tile so CJK/emoji glyphs
    /// aren't clipped to their lead cell; the rect is cached per (char, style).
    #[test]
    fn wide_chars_get_two_cell_tiles_and_cache_hits() {
        let Some((mut core, mut font)) = headless_core() else { return };
        let (wide, _) = core.tile_for_char('好', Style::Regular, &mut font);
        assert_eq!(wide.2, 2 * core.cell_w, "wide glyph tile spans two cells");
        let (narrow, _) = core.tile_for_char('a', Style::Regular, &mut font);
        assert_eq!(narrow.2, core.cell_w, "narrow glyph tile spans one cell");
        // Same key returns the cached rect without a new allocation.
        let shelf = (core.shelf_x, core.shelf_y);
        let (again, _) = core.tile_for_char('好', Style::Regular, &mut font);
        assert_eq!(again, wide);
        assert_eq!((core.shelf_x, core.shelf_y), shelf);
    }

    #[test]
    fn set_opacity_clamps_to_unit_range() {
        let Some(bytes) = super::super::font::load_default_font(None) else {
            eprintln!("no system font; skipping GPU core test");
            return;
        };
        let mut font = FontCache::new(super::super::font::FontSet { regular: bytes, ..Default::default() }, 16.0, false).unwrap();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let Some(mut core) = GpuCore::new(&instance, None, &mut font) else {
            eprintln!("no wgpu adapter; skipping GPU core test");
            return;
        };
        assert_eq!(core.opacity, 1.0);
        core.set_opacity(0.5);
        assert_eq!(core.opacity, 0.5);
        core.set_opacity(-1.0);
        assert_eq!(core.opacity, 0.0);
        core.set_opacity(2.0);
        assert_eq!(core.opacity, 1.0);
    }

    /// Full render-to-texture + readback: a blank blue cell should read back as
    /// blue. Ignored by default because the software drivers in headless CI/WSL
    /// (lavapipe, dzn) segfault during draw/submit; run with `--ignored` on a
    /// machine with a working GPU/Vulkan adapter.
    #[test]
    #[ignore = "render+readback needs a working GPU adapter; lavapipe/dzn crash headless"]
    fn gpu_renders_to_texture() {
        let bytes = super::super::font::load_default_font(None).unwrap();
        let mut font = FontCache::new(super::super::font::FontSet { regular: bytes, ..Default::default() }, 16.0, false).unwrap();
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
