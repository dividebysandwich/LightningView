// --- SDL_GPU immediate-mode 2D renderer ---
//
// Owns the SDL window and the single `sdl3::gpu::Device`, and exposes a small
// immediate-mode drawing API (textured quads, solid rects, text) that the app's
// render pass builds each frame. All geometry is batched into one dynamic vertex
// buffer and flushed in a single render pass.
//
// A single graphics pipeline + fragment shader (`shaders/quad.frag`) serves every
// 2D draw: textured images sample an RGBA texture, solid rects sample a 1x1 white
// texture, and text samples an RGBA coverage texture — in every case the sampled
// value is multiplied by a per-vertex colour.
//
// The `Device` must outlive every `GpuTexture` (textures hold a weak ref to it and
// free themselves on drop), so `Renderer` owns both and is dropped last.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use sdl3::gpu::{
    BlendFactor, BlendOp, Buffer, BufferBinding, BufferRegion, BufferUsageFlags,
    ColorTargetBlendState, ColorTargetDescription, ColorTargetInfo, Device, Filter,
    GraphicsPipeline, GraphicsPipelineTargetInfo, LoadOp, PresentMode, PrimitiveType, Sampler,
    SamplerAddressMode, SamplerCreateInfo, ShaderFormat, ShaderStage, StoreOp, SwapchainComposition,
    Texture, TextureCreateInfo, TextureRegion, TextureSamplerBinding, TextureTransferInfo,
    TextureType, TextureUsage, TransferBufferLocation, TransferBufferUsage, VertexAttribute,
    VertexBufferDescription, VertexElementFormat, VertexInputRate, VertexInputState,
};
use sdl3::pixels::Color;
use sdl3::video::Window;
use sdl3::VideoSubsystem;

use crate::geom::{Rect, Vec2};
use crate::types::PixelBuf;

// Per-shader bytecode in every backend format SDL_GPU might want: SPIR-V
// (Vulkan), DXIL (D3D12), MSL (Metal). build.rs always produces the SPIR-V from
// the GLSL source, then transpiles it to DXIL/MSL via SDL_shadercross when that
// tool is available (the CI Windows/macOS jobs install it). When a format wasn't
// produced, build.rs writes an empty placeholder so `include_bytes!` still
// compiles, and the renderer simply doesn't advertise that format.
struct ShaderBlobs {
    spirv: &'static [u8],
    dxil: &'static [u8],
    msl: &'static [u8],
}

macro_rules! shader_blobs {
    ($name:literal) => {
        ShaderBlobs {
            spirv: include_bytes!(concat!(env!("OUT_DIR"), "/", $name, ".spv")),
            dxil: include_bytes!(concat!(env!("OUT_DIR"), "/", $name, ".dxil")),
            msl: include_bytes!(concat!(env!("OUT_DIR"), "/", $name, ".msl")),
        }
    };
}

const QUAD_VERT: ShaderBlobs = shader_blobs!("quad.vert");
const QUAD_FRAG: ShaderBlobs = shader_blobs!("quad.frag");
const VIDEO_FRAG: ShaderBlobs = shader_blobs!("video.frag");

/// The shader formats we can actually supply, i.e. those whose blobs are present.
/// SPIR-V is the always-available baseline; DXIL/MSL are added only when built.
fn available_shader_formats() -> ShaderFormat {
    let mut f = ShaderFormat::SPIRV;
    if !QUAD_VERT.dxil.is_empty() {
        f = f | ShaderFormat::DXIL;
    }
    if !QUAD_VERT.msl.is_empty() {
        f = f | ShaderFormat::MSL;
    }
    f
}

/// Human-readable list of the shader formats in `f`, naming the backend each
/// implies (the raw `ShaderFormat(n)` bitmask is opaque in `/debug` logs).
fn describe_shader_formats(f: ShaderFormat) -> String {
    let has = |bit: ShaderFormat| f & bit == bit;
    let names: Vec<&str> = [
        (ShaderFormat::SPIRV, "SPIRV(Vulkan)"),
        (ShaderFormat::DXIL, "DXIL(D3D12)"),
        (ShaderFormat::MSL, "MSL(Metal)"),
    ]
    .into_iter()
    .filter(|(bit, _)| has(*bit))
    .map(|(_, name)| name)
    .collect();
    if names.is_empty() {
        format!("{f:?}")
    } else {
        names.join(" | ")
    }
}

/// Pick the bytecode + format + entry point for `blobs` matching what the device
/// consumes (`device.get_shader_formats()`). Prefers the platform-native format
/// (DXIL on D3D12, MSL on Metal) and falls back to SPIR-V (Vulkan).
fn select_shader(
    blobs: &ShaderBlobs,
    device_formats: ShaderFormat,
) -> Result<(&'static [u8], ShaderFormat, &'static std::ffi::CStr)> {
    let has = |fmt: ShaderFormat| device_formats & fmt == fmt;
    if has(ShaderFormat::DXIL) && !blobs.dxil.is_empty() {
        Ok((blobs.dxil, ShaderFormat::DXIL, c"main"))
    } else if has(ShaderFormat::MSL) && !blobs.msl.is_empty() {
        // SPIRV-Cross (used by shadercross) renames the entry point for MSL.
        Ok((blobs.msl, ShaderFormat::MSL, c"main0"))
    } else if has(ShaderFormat::SPIRV) && !blobs.spirv.is_empty() {
        Ok((blobs.spirv, ShaderFormat::SPIRV, c"main"))
    } else {
        Err(anyhow!(
            "no shader available for this GPU backend (device formats {device_formats:?}); \
             the DXIL/MSL shaders may not have been built (SDL_shadercross missing at build time)"
        ))
    }
}

/// RGBA colour with components in `0.0..=1.0`.
pub type Rgba = [f32; 4];

/// Convert 8-bit RGBA to the float colour the renderer multiplies into vertices.
#[inline]
pub fn rgba8(r: u8, g: u8, b: u8, a: u8) -> Rgba {
    [
        r as f32 / 255.0,
        g as f32 / 255.0,
        b as f32 / 255.0,
        a as f32 / 255.0,
    ]
}

#[inline]
pub fn gray(v: u8) -> Rgba {
    rgba8(v, v, v, 255)
}

pub const WHITE: Rgba = [1.0, 1.0, 1.0, 1.0];

/// A GPU-resident RGBA texture. Cheap to clone (the inner SDL texture is Arc'd).
#[derive(Clone)]
pub struct GpuTexture {
    tex: Texture<'static>,
    size: [u32; 2],
}

impl GpuTexture {
    #[inline]
    #[allow(dead_code)]
    pub fn size(&self) -> [u32; 2] {
        self.size
    }
    #[inline]
    pub fn width(&self) -> u32 {
        self.size[0]
    }
    #[inline]
    pub fn height(&self) -> u32 {
        self.size[1]
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Vertex {
    pos: [f32; 2], // normalised device coordinates
    uv: [f32; 2],
    color: [f32; 4],
}

/// One batched draw: a contiguous run of vertices sampling a single texture.
struct DrawCmd {
    tex: Texture<'static>,
    first: u32,
    count: u32,
}

/// Colour description for a video frame, supplied by the caller and packed into
/// the video shader's uniform. Drives YUV matrix, range, and HDR tone-mapping.
#[derive(Clone, Copy)]
pub struct VideoColorParams {
    /// 0 = SDR, 1 = PQ, 2 = HLG.
    pub transfer: i32,
    /// BT.2020 primaries (true) vs BT.709 (false).
    pub bt2020: bool,
    pub full_range: bool,
    pub peak_nits: f32,
    pub sdr_white_nits: f32,
}

/// Reference SDR diffuse-white luminance (BT.2408) used both as the HDR→SDR
/// tone-map target and as the level the SDR UI sits at on an HDR swapchain.
const DISPLAY_SDR_WHITE_NITS: f32 = 203.0;

/// std140-compatible layout pushed as the video fragment uniform (two vec4s).
#[repr(C)]
#[derive(Clone, Copy)]
struct VideoUniforms {
    mode: [i32; 4], // transfer, bt2020, full_range, output_mode
    lum: [f32; 4],  // src_peak_nits, sdr_tonemap_white, display_sdr_white, unused
}

/// std140-compatible layout pushed as the overlay (quad) fragment uniform.
#[repr(C)]
#[derive(Clone, Copy)]
struct QuadUniforms {
    mode: [i32; 4], // output_mode, unused...
    lum: [f32; 4],  // display_sdr_white, unused...
}

/// The current frame's video quad, drawn before the overlay batch using the
/// dedicated YUV pipeline (two plane samplers + a colour uniform).
struct VideoDraw {
    y: Texture<'static>,
    uv: Texture<'static>,
    first: u32,
    params: VideoColorParams,
}

/// Horizontal text anchor for `draw_text`.
#[derive(Clone, Copy, PartialEq)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

pub struct Renderer {
    // NOTE: Rust drops struct fields in declaration order. Every GPU resource
    // below (pipelines, sampler, textures, buffers, cached text textures) holds a
    // weak ref to the device and only releases itself to the driver while the
    // device is still alive — so `device` (and the `window` it claimed) must be
    // declared LAST, after all resources, to avoid leaking them on shutdown.
    pipeline: GraphicsPipeline,
    /// Pipeline for NV12 video frames (samples Y + UV planes, YUV->RGB in shader).
    video_pipeline: GraphicsPipeline,
    sampler: Sampler,
    /// 1x1 opaque white texture used to draw solid-colour rects.
    white: GpuTexture,
    clear_color: Color,
    /// Active swapchain composition (SDR vs an HDR mode). Drives the shader
    /// output encoding; changing it rebuilds the pipelines for the new format.
    composition: SwapchainComposition,
    /// Last-logged HDR capability tuple (display_hdr, hdr10_ok, scrgb_ok), so we
    /// report capability changes once instead of every frame.
    hdr_diag: Option<(bool, bool, bool)>,
    /// `LV_FORCE_HDR` test override: when set, forces this composition for HDR
    /// content regardless of detected display capability.
    force_hdr: Option<SwapchainComposition>,

    // Per-frame geometry, rebuilt each frame.
    verts: Vec<Vertex>,
    cmds: Vec<DrawCmd>,
    /// Optional video frame to draw beneath the overlay batch this frame.
    video_draw: Option<VideoDraw>,
    drawable: (f32, f32),

    // Persistent dynamic vertex/transfer buffers, grown as needed.
    vertex_buffer: Option<Buffer>,
    transfer_buffer: Option<sdl3::gpu::TransferBuffer>,
    buffer_cap: u32,

    // Text rendering.
    font: Option<fontdue::Font>,
    /// Cache of rasterised single-line strings, keyed by (text, rounded px size).
    text_cache: HashMap<(String, u32), CachedText>,
    frame_counter: u64,

    // Dropped last (see note above).
    device: Device,
    window: Window,
}

struct CachedText {
    tex: GpuTexture,
    last_used: u64,
}

impl Renderer {
    pub fn new(
        video: &VideoSubsystem,
        title: &str,
        width: u32,
        height: u32,
        fullscreen: bool,
    ) -> Result<Self> {
        let mut builder = video.window(title, width, height);
        // Note: HiDPI (high_pixel_density) is intentionally not requested yet, so
        // logical mouse coordinates match the pixel drawable size and the zoom/pan
        // math needs no DPI scaling. HiDPI support can be layered on later.
        builder.position_centered().resizable();
        if fullscreen {
            builder.fullscreen();
        }
        let window = builder.build().map_err(|e| anyhow!("create window: {e}"))?;

        // Advertise every shader format we actually shipped blobs for, so SDL can
        // pick its preferred backend per platform: D3D12 (DXIL) on Windows, Metal
        // (MSL) on macOS, Vulkan (SPIR-V) on Linux. If only SPIR-V is present
        // (e.g. DXIL/MSL weren't built), this still works wherever Vulkan exists.
        let formats = available_shader_formats();
        let device = Device::new(formats, cfg!(debug_assertions))
            .map_err(|e| {
                anyhow!(
                    "create GPU device: {e}. No supported GPU backend was found for the \
                     available shader formats ({formats:?}); update your GPU drivers."
                )
            })?
            .with_window(&window)
            .map_err(|e| anyhow!("claim window for GPU: {e}"))?;

        // VSync paces presentation; SDR swapchain for now (HDR comes in a later phase).
        let _ = device.set_swapchain_parameters(
            &window,
            PresentMode::Vsync,
            sdl3::gpu::SwapchainComposition::Sdr,
        );

        let pipeline = build_pipeline(&device, &window, &QUAD_FRAG, 1, 1)?;
        let video_pipeline = build_pipeline(&device, &window, &VIDEO_FRAG, 2, 1)?;

        let sampler = device
            .create_sampler(
                SamplerCreateInfo::new()
                    .with_min_filter(Filter::Linear)
                    .with_mag_filter(Filter::Linear)
                    .with_address_mode_u(SamplerAddressMode::ClampToEdge)
                    .with_address_mode_v(SamplerAddressMode::ClampToEdge)
                    .with_address_mode_w(SamplerAddressMode::ClampToEdge),
            )
            .map_err(|e| anyhow!("create sampler: {e}"))?;

        // 1x1 opaque white texture, used to draw solid-colour rects/outlines.
        let white = upload_rgba(&device, 1, 1, &[255, 255, 255, 255])?;

        log_displays();
        // Startup HDR/back-end snapshot for `/debug`: which shader bytecode the
        // device actually selected (DXIL=D3D12, MSL=Metal, SPIRV=Vulkan) and
        // whether the current window's display can present HDR — so HDR support
        // can be diagnosed without having to play an HDR clip first.
        log::debug!(
            "GPU device selected shader formats: {}",
            describe_shader_formats(device.get_shader_formats())
        );
        log::debug!(
            "Window HDR at startup: enabled={} hdr10_supported={} scrgb_supported={}",
            window_hdr_enabled(&window),
            supports_composition(&device, &window, SwapchainComposition::Hdr10St2084),
            supports_composition(&device, &window, SwapchainComposition::HdrExtendedLinear),
        );
        let force_hdr = forced_hdr_composition();
        if let Some(c) = force_hdr {
            log::info!("LV_FORCE_HDR set: forcing {c:?} for HDR content.");
        }

        Ok(Self {
            window,
            device,
            pipeline,
            video_pipeline,
            sampler,
            white,
            clear_color: Color::RGB(20, 20, 20),
            composition: SwapchainComposition::Sdr,
            hdr_diag: None,
            force_hdr,
            verts: Vec::new(),
            cmds: Vec::new(),
            video_draw: None,
            drawable: (width as f32, height as f32),
            vertex_buffer: None,
            transfer_buffer: None,
            buffer_cap: 0,
            font: load_system_font(),
            text_cache: HashMap::new(),
            frame_counter: 0,
        })
    }

    #[allow(dead_code)]
    pub fn window(&self) -> &Window {
        &self.window
    }

    /// Drawable size in pixels (accounts for HiDPI scaling).
    pub fn drawable_size(&self) -> Vec2 {
        let (w, h) = self.window.size_in_pixels();
        Vec2::new(w as f32, h as f32)
    }

    /// Shader output-encoding mode for the active swapchain composition:
    /// 0 = SDR sRGB, 1 = HDR10 PQ (BT.2020), 2 = scRGB extended-linear.
    fn output_mode(&self) -> i32 {
        match self.composition {
            SwapchainComposition::Hdr10St2084 => 1,
            SwapchainComposition::HdrExtendedLinear => 2,
            _ => 0,
        }
    }

    /// Choose the swapchain composition for the current display + content, and
    /// reconfigure if it changed. Called every frame: HDR engages only while HDR
    /// content plays on an HDR-capable display, and (because the window's HDR
    /// state is re-read each call) it follows the window across a multi-monitor
    /// setup — dropping back to SDR tone-mapping on a non-HDR screen.
    pub fn update_hdr_output(&mut self, content_is_hdr: bool) {
        let hdr_display = window_hdr_enabled(&self.window);
        let (sup_hdr10, sup_scrgb) = if content_is_hdr && hdr_display {
            (
                supports_composition(&self.device, &self.window, SwapchainComposition::Hdr10St2084),
                supports_composition(
                    &self.device,
                    &self.window,
                    SwapchainComposition::HdrExtendedLinear,
                ),
            )
        } else {
            (false, false)
        };

        // Report the capability picture once per change while HDR content plays,
        // so it's clear whether passthrough is available on the current monitor.
        if content_is_hdr {
            let diag = (hdr_display, sup_hdr10, sup_scrgb);
            if self.hdr_diag != Some(diag) {
                self.hdr_diag = Some(diag);
                log::info!(
                    "HDR content: display_hdr={hdr_display} hdr10_supported={sup_hdr10} \
                     scrgb_supported={sup_scrgb}"
                );
            }
        } else {
            self.hdr_diag = None;
        }

        let desired = match self.force_hdr {
            // Test override: force the requested composition for HDR content.
            Some(forced) if content_is_hdr => forced,
            _ if sup_hdr10 => SwapchainComposition::Hdr10St2084,
            _ if sup_scrgb => SwapchainComposition::HdrExtendedLinear,
            _ => SwapchainComposition::Sdr,
        };

        if desired != self.composition {
            self.reconfigure_swapchain(desired);
        }
    }

    /// Switch the swapchain composition and rebuild the pipelines to match the
    /// new colour-target format. Falls back to SDR on any failure.
    fn reconfigure_swapchain(&mut self, comp: SwapchainComposition) {
        if let Err(e) =
            self.device
                .set_swapchain_parameters(&self.window, PresentMode::Vsync, comp)
        {
            log::warn!("Could not set swapchain composition {comp:?}: {e}");
            if comp != SwapchainComposition::Sdr {
                self.reconfigure_swapchain(SwapchainComposition::Sdr);
            }
            return;
        }

        // The swapchain texture format changes with composition (e.g. R10G10B10A2
        // for HDR10, RGBA16F for scRGB), so the pipelines must be rebuilt to match.
        let quad = build_pipeline(&self.device, &self.window, &QUAD_FRAG, 1, 1);
        let video = build_pipeline(&self.device, &self.window, &VIDEO_FRAG, 2, 1);
        match (quad, video) {
            (Ok(q), Ok(v)) => {
                self.pipeline = q;
                self.video_pipeline = v;
                self.composition = comp;
                log::info!(
                    "Swapchain composition -> {comp:?} (output mode {})",
                    self.output_mode()
                );
            }
            (q, v) => {
                log::error!(
                    "Failed to rebuild pipelines for {comp:?}: {:?} / {:?}; reverting.",
                    q.err(),
                    v.err()
                );
                let _ = self.device.set_swapchain_parameters(
                    &self.window,
                    PresentMode::Vsync,
                    self.composition,
                );
            }
        }
    }

    pub fn set_fullscreen(&mut self, fullscreen: bool) {
        let _ = self.window.set_fullscreen(fullscreen);
    }

    pub fn is_fullscreen(&self) -> bool {
        use sdl3::video::FullscreenType;
        self.window.fullscreen_state() != FullscreenType::Off
    }

    /// Upload a CPU pixel buffer to a new GPU texture.
    pub fn upload_texture(&self, buf: &PixelBuf) -> Result<GpuTexture> {
        upload_rgba(&self.device, buf.size[0], buf.size[1], &buf.rgba)
    }

    // --- Per-frame drawing ---------------------------------------------------

    pub fn begin_frame(&mut self) {
        self.verts.clear();
        self.cmds.clear();
        self.video_draw = None;
        self.drawable = {
            let (w, h) = self.window.size_in_pixels();
            (w.max(1) as f32, h.max(1) as f32)
        };
        self.frame_counter += 1;
    }

    /// Append the 6 vertices for a quad to the shared buffer (no draw command).
    /// Returns the index of the first appended vertex.
    fn append_quad_verts(&mut self, dst: Rect, uv: Rect, color: Rgba) -> u32 {
        let (w, h) = self.drawable;
        let to_ndc = |x: f32, y: f32| [x / w * 2.0 - 1.0, 1.0 - y / h * 2.0];

        let x0 = dst.min.x;
        let y0 = dst.min.y;
        let x1 = dst.min.x + dst.size.x;
        let y1 = dst.min.y + dst.size.y;
        let u0 = uv.min.x;
        let v0 = uv.min.y;
        let u1 = uv.min.x + uv.size.x;
        let v1 = uv.min.y + uv.size.y;

        let tl = Vertex { pos: to_ndc(x0, y0), uv: [u0, v0], color };
        let tr = Vertex { pos: to_ndc(x1, y0), uv: [u1, v0], color };
        let bl = Vertex { pos: to_ndc(x0, y1), uv: [u0, v1], color };
        let br = Vertex { pos: to_ndc(x1, y1), uv: [u1, v1], color };

        let first = self.verts.len() as u32;
        self.verts.extend_from_slice(&[tl, tr, bl, bl, tr, br]);
        first
    }

    fn push_quad(&mut self, tex: Texture<'static>, dst: Rect, uv: Rect, color: Rgba) {
        let first = self.append_quad_verts(dst, uv, color);
        // Coalesce consecutive draws of the same texture into one command.
        if let Some(last) = self.cmds.last_mut() {
            if last.tex.raw() == tex.raw() {
                last.count += 6;
                return;
            }
        }
        self.cmds.push(DrawCmd { tex, first, count: 6 });
    }

    /// Draw a YUV video frame (Y + UV plane textures) into `dst`, beneath this
    /// frame's overlay batch. The video shader converts YUV->RGB and, for HDR
    /// (`params.transfer != 0`), tone-maps to SDR.
    pub fn draw_video(
        &mut self,
        y: &GpuTexture,
        uv: &GpuTexture,
        dst: Rect,
        params: VideoColorParams,
    ) {
        let first = self.append_quad_verts(dst, full_uv(), WHITE);
        self.video_draw = Some(VideoDraw {
            y: y.tex.clone(),
            uv: uv.tex.clone(),
            first,
            params,
        });
    }

    /// Upload a single-channel 8-bit (R8) plane — an NV12 luma plane.
    pub fn upload_r8(&self, w: u32, h: u32, bytes: &[u8]) -> Result<GpuTexture> {
        upload_plane(&self.device, w, h, bytes, sdl3::gpu::TextureFormat::R8Unorm, 1)
    }

    /// Upload a two-channel 8-bit (R8G8) plane — an NV12 interleaved chroma plane.
    pub fn upload_r8g8(&self, w: u32, h: u32, bytes: &[u8]) -> Result<GpuTexture> {
        upload_plane(&self.device, w, h, bytes, sdl3::gpu::TextureFormat::R8g8Unorm, 2)
    }

    /// Upload a single-channel 16-bit (R16) plane — a P010 luma plane.
    pub fn upload_r16(&self, w: u32, h: u32, bytes: &[u8]) -> Result<GpuTexture> {
        upload_plane(&self.device, w, h, bytes, sdl3::gpu::TextureFormat::R16Unorm, 2)
    }

    /// Upload a two-channel 16-bit (R16G16) plane — a P010 interleaved chroma plane.
    pub fn upload_r16g16(&self, w: u32, h: u32, bytes: &[u8]) -> Result<GpuTexture> {
        upload_plane(&self.device, w, h, bytes, sdl3::gpu::TextureFormat::R16g16Unorm, 4)
    }

    /// Draw a texture into `dst` (pixel space). `uv` selects the source region
    /// (use `Rect::from_min_max(Vec2::ZERO, Vec2::new(1.0,1.0))` for the whole texture).
    pub fn draw_texture(&mut self, tex: &GpuTexture, dst: Rect, uv: Rect, color: Rgba) {
        self.push_quad(tex.tex.clone(), dst, uv, color);
    }

    /// Draw a texture filling `dst` using its entire area.
    pub fn draw_texture_full(&mut self, tex: &GpuTexture, dst: Rect, color: Rgba) {
        self.draw_texture(tex, dst, full_uv(), color);
    }

    /// Fill a solid-colour rectangle.
    pub fn fill_rect(&mut self, dst: Rect, color: Rgba) {
        let white = self.white.tex.clone();
        self.push_quad(white, dst, full_uv(), color);
    }

    /// Draw a 1px-ish stroked rectangle outline of the given thickness.
    pub fn stroke_rect(&mut self, dst: Rect, thickness: f32, color: Rgba) {
        let t = thickness.max(1.0);
        let Rect { min, size } = dst;
        // top, bottom, left, right
        self.fill_rect(Rect::xywh(min.x, min.y, size.x, t), color);
        self.fill_rect(Rect::xywh(min.x, min.y + size.y - t, size.x, t), color);
        self.fill_rect(Rect::xywh(min.x, min.y, t, size.y), color);
        self.fill_rect(Rect::xywh(min.x + size.x - t, min.y, t, size.y), color);
    }

    /// Measure a single line of text at the given pixel size. Returns (w, h).
    pub fn text_size(&self, text: &str, px: f32) -> Vec2 {
        match &self.font {
            Some(font) => {
                let (w, h, _) = measure_line(font, text, px);
                Vec2::new(w as f32, h as f32)
            }
            None => Vec2::new(0.0, 0.0),
        }
    }

    /// Draw a single line of text. `pos` is the anchor point; `align` controls
    /// horizontal anchoring and the text is vertically top-aligned at `pos.y`.
    pub fn draw_text(&mut self, text: &str, px: f32, pos: Vec2, align: TextAlign, color: Rgba) {
        if text.is_empty() || self.font.is_none() {
            return;
        }
        let key = (text.to_string(), px.round() as u32);
        let cached = if let Some(c) = self.text_cache.get(&key) {
            c.tex.clone()
        } else {
            let font = self.font.as_ref().unwrap();
            let buf = rasterize_line(font, text, px);
            let Some(buf) = buf else { return };
            let Ok(tex) = upload_rgba(&self.device, buf.size[0], buf.size[1], &buf.rgba) else {
                return;
            };
            tex
        };
        // Insert/refresh cache entry (re-borrow to satisfy the borrow checker).
        self.text_cache.insert(
            key,
            CachedText { tex: cached.clone(), last_used: self.frame_counter },
        );

        let w = cached.width() as f32;
        let h = cached.height() as f32;
        let x = match align {
            TextAlign::Left => pos.x,
            TextAlign::Center => pos.x - w / 2.0,
            TextAlign::Right => pos.x - w,
        };
        let dst = Rect::xywh(x, pos.y, w, h);
        self.draw_texture(&cached, dst, full_uv(), color);
    }

    /// Draw a line of text with a black outline (matching the old subtitle style),
    /// then the fill colour on top.
    pub fn draw_text_outlined(&mut self, text: &str, px: f32, pos: Vec2, align: TextAlign, color: Rgba) {
        let o = 1.5;
        for (dx, dy) in [(-o, 0.0), (o, 0.0), (0.0, -o), (0.0, o)] {
            self.draw_text(text, px, Vec2::new(pos.x + dx, pos.y + dy), align, [0.0, 0.0, 0.0, color[3]]);
        }
        self.draw_text(text, px, pos, align, color);
    }

    pub fn end_frame(&mut self) -> Result<()> {
        let verts = std::mem::take(&mut self.verts);
        let cmds = std::mem::take(&mut self.cmds);
        let video_draw = self.video_draw.take();
        let output_mode = self.output_mode();
        // Overlay (image/text/rect) uniform: tells the quad shader how to encode
        // sRGB UI for the active swapchain.
        let quad_uniforms = QuadUniforms {
            mode: [output_mode, 0, 0, 0],
            lum: [DISPLAY_SDR_WHITE_NITS, 0.0, 0.0, 0.0],
        };
        // Video uniform combines the frame's colour params with the output mode.
        let video_uniforms = video_draw.as_ref().map(|vd| VideoUniforms {
            mode: [
                vd.params.transfer,
                vd.params.bt2020 as i32,
                vd.params.full_range as i32,
                output_mode,
            ],
            lum: [
                vd.params.peak_nits,
                vd.params.sdr_white_nits,
                DISPLAY_SDR_WHITE_NITS,
                0.0,
            ],
        });
        // Evict text-cache entries unused for a while to bound memory.
        let fc = self.frame_counter;
        self.text_cache.retain(|_, c| fc.saturating_sub(c.last_used) < 240);

        let nbytes = (verts.len() * std::mem::size_of::<Vertex>()) as u32;
        if nbytes > 0 {
            self.ensure_buffer_capacity(nbytes)?;
            let tb = self.transfer_buffer.as_ref().unwrap();
            let mut map = tb.map::<Vertex>(&self.device, true);
            let dst = map.mem_mut();
            dst[..verts.len()].copy_from_slice(&verts);
            map.unmap();
        }

        let mut cmd = self
            .device
            .acquire_command_buffer()
            .map_err(|e| anyhow!("acquire cmd buffer: {e}"))?;

        if nbytes > 0 {
            let copy = self
                .device
                .begin_copy_pass(&cmd)
                .map_err(|e| anyhow!("begin copy pass: {e}"))?;
            copy.upload_to_gpu_buffer(
                TransferBufferLocation::new()
                    .with_transfer_buffer(self.transfer_buffer.as_ref().unwrap())
                    .with_offset(0),
                BufferRegion::new()
                    .with_buffer(self.vertex_buffer.as_ref().unwrap())
                    .with_offset(0)
                    .with_size(nbytes),
                true,
            );
            self.device.end_copy_pass(copy);
        }

        match cmd.wait_and_acquire_swapchain_texture(&self.window) {
            Ok(swapchain) => {
                let color_targets = [ColorTargetInfo::default()
                    .with_texture(&swapchain)
                    .with_load_op(LoadOp::CLEAR)
                    .with_store_op(StoreOp::STORE)
                    .with_clear_color(self.clear_color)];
                let pass = self
                    .device
                    .begin_render_pass(&cmd, &color_targets, None)
                    .map_err(|e| anyhow!("begin render pass: {e}"))?;

                if nbytes > 0 {
                    pass.bind_vertex_buffers(
                        0,
                        &[BufferBinding::new()
                            .with_buffer(self.vertex_buffer.as_ref().unwrap())
                            .with_offset(0)],
                    );

                    // Video frame first (beneath the overlays), via the YUV pipeline.
                    if let (Some(vd), Some(vu)) = (&video_draw, &video_uniforms) {
                        pass.bind_graphics_pipeline(&self.video_pipeline);
                        cmd.push_fragment_uniform_data(0, vu);
                        pass.bind_fragment_samplers(
                            0,
                            &[
                                TextureSamplerBinding::new()
                                    .with_texture(&vd.y)
                                    .with_sampler(&self.sampler),
                                TextureSamplerBinding::new()
                                    .with_texture(&vd.uv)
                                    .with_sampler(&self.sampler),
                            ],
                        );
                        pass.draw_primitives(6, 1, vd.first as usize, 0);
                    }

                    // Then the overlay batch (images, text, rects) via the quad pipeline.
                    pass.bind_graphics_pipeline(&self.pipeline);
                    cmd.push_fragment_uniform_data(0, &quad_uniforms);
                    for c in &cmds {
                        pass.bind_fragment_samplers(
                            0,
                            &[TextureSamplerBinding::new()
                                .with_texture(&c.tex)
                                .with_sampler(&self.sampler)],
                        );
                        pass.draw_primitives(c.count as usize, 1, c.first as usize, 0);
                    }
                }
                self.device.end_render_pass(pass);
                cmd.submit().map_err(|e| anyhow!("submit frame: {e}"))?;
            }
            Err(_) => {
                cmd.cancel();
            }
        }
        Ok(())
    }

    fn ensure_buffer_capacity(&mut self, bytes: u32) -> Result<()> {
        if self.buffer_cap >= bytes && self.vertex_buffer.is_some() {
            return Ok(());
        }
        let cap = bytes.next_power_of_two().max(64 * 1024);
        let vbuf = self
            .device
            .create_buffer()
            .with_size(cap)
            .with_usage(BufferUsageFlags::VERTEX)
            .build()
            .map_err(|e| anyhow!("create vertex buffer: {e}"))?;
        let tbuf = self
            .device
            .create_transfer_buffer()
            .with_size(cap)
            .with_usage(TransferBufferUsage::UPLOAD)
            .build()
            .map_err(|e| anyhow!("create vertex transfer buffer: {e}"))?;
        self.vertex_buffer = Some(vbuf);
        self.transfer_buffer = Some(tbuf);
        self.buffer_cap = cap;
        Ok(())
    }
}

fn full_uv() -> Rect {
    Rect::from_min_max(Vec2::ZERO, Vec2::new(1.0, 1.0))
}

/// True if the display the window currently occupies has HDR enabled. Re-read
/// each frame so moving the window between monitors is picked up automatically.
fn window_hdr_enabled(window: &Window) -> bool {
    unsafe {
        let props = sdl3::sys::video::SDL_GetWindowProperties(window.raw());
        sdl3::sys::properties::SDL_GetBooleanProperty(
            props,
            sdl3::sys::video::SDL_PROP_WINDOW_HDR_ENABLED_BOOLEAN,
            false,
        )
    }
}

/// Whether the GPU device + the window's current display support presenting in
/// the given swapchain composition.
fn supports_composition(device: &Device, window: &Window, comp: SwapchainComposition) -> bool {
    unsafe {
        sdl3::sys::gpu::SDL_WindowSupportsGPUSwapchainComposition(
            device.raw(),
            window.raw(),
            sdl3::sys::gpu::SDL_GPUSwapchainComposition(comp as i32),
        )
    }
}

/// Log each connected display and whether SDL reports HDR enabled on it — handy
/// for diagnosing which monitor in a multi-screen setup supports HDR passthrough.
fn log_displays() {
    unsafe {
        let mut count: i32 = 0;
        let ids = sdl3::sys::video::SDL_GetDisplays(&mut count);
        if ids.is_null() {
            return;
        }
        for i in 0..count as isize {
            let id = *ids.offset(i);
            let props = sdl3::sys::video::SDL_GetDisplayProperties(id);
            let hdr = sdl3::sys::properties::SDL_GetBooleanProperty(
                props,
                sdl3::sys::video::SDL_PROP_DISPLAY_HDR_ENABLED_BOOLEAN,
                false,
            );
            let name_ptr = sdl3::sys::video::SDL_GetDisplayName(id);
            let name = if name_ptr.is_null() {
                String::from("?")
            } else {
                std::ffi::CStr::from_ptr(name_ptr).to_string_lossy().into_owned()
            };
            log::info!("Display {}: \"{name}\" hdr_enabled={hdr}", id.0);
        }
        sdl3::sys::stdinc::SDL_free(ids as *mut std::ffi::c_void);
    }
}

/// Read the `LV_FORCE_HDR` test override: forces a swapchain composition for HDR
/// content regardless of detected display capability. Useful for validating the
/// passthrough render path on an HDR screen when auto-detection is unavailable.
fn forced_hdr_composition() -> Option<SwapchainComposition> {
    match std::env::var("LV_FORCE_HDR").ok()?.to_lowercase().as_str() {
        "hdr10" | "pq" | "1" => Some(SwapchainComposition::Hdr10St2084),
        "scrgb" | "linear" | "2" => Some(SwapchainComposition::HdrExtendedLinear),
        _ => None,
    }
}

/// Create a GPU texture and upload tightly-packed RGBA8 bytes into it.
fn upload_rgba(device: &Device, w: u32, h: u32, bytes: &[u8]) -> Result<GpuTexture> {
    upload_plane(device, w, h, bytes, sdl3::gpu::TextureFormat::R8g8b8a8Unorm, 4)
}

/// Create a GPU texture of `format` (with `bpp` bytes per texel) and upload
/// tightly-packed bytes into it. Runs its own one-shot copy-pass command buffer,
/// so callers only need `&Device`. Used for RGBA images and NV12 video planes.
fn upload_plane(
    device: &Device,
    w: u32,
    h: u32,
    bytes: &[u8],
    format: sdl3::gpu::TextureFormat,
    bpp: u32,
) -> Result<GpuTexture> {
    let w = w.max(1);
    let h = h.max(1);
    let tex = device
        .create_texture(
            TextureCreateInfo::new()
                .with_format(format)
                .with_type(TextureType::_2D)
                .with_width(w)
                .with_height(h)
                .with_layer_count_or_depth(1)
                .with_num_levels(1)
                .with_usage(TextureUsage::SAMPLER),
        )
        .map_err(|e| anyhow!("create texture {w}x{h}: {e}"))?;

    let size_bytes = w * h * bpp;
    let tb = device
        .create_transfer_buffer()
        .with_size(size_bytes)
        .with_usage(TransferBufferUsage::UPLOAD)
        .build()
        .map_err(|e| anyhow!("create transfer buffer: {e}"))?;
    {
        let mut map = tb.map::<u8>(device, false);
        let dst = map.mem_mut();
        let n = bytes.len().min(dst.len());
        dst[..n].copy_from_slice(&bytes[..n]);
        map.unmap();
    }

    let cmd = device
        .acquire_command_buffer()
        .map_err(|e| anyhow!("acquire cmd buffer: {e}"))?;
    let copy = device
        .begin_copy_pass(&cmd)
        .map_err(|e| anyhow!("begin copy pass: {e}"))?;
    copy.upload_to_gpu_texture(
        TextureTransferInfo::new().with_transfer_buffer(&tb).with_offset(0),
        TextureRegion::new()
            .with_texture(&tex)
            .with_layer(0)
            .with_width(w)
            .with_height(h)
            .with_depth(1),
        false,
    );
    device.end_copy_pass(copy);
    cmd.submit().map_err(|e| anyhow!("submit upload: {e}"))?;

    Ok(GpuTexture { tex, size: [w, h] })
}

fn build_pipeline(
    device: &Device,
    window: &Window,
    frag: &ShaderBlobs,
    num_samplers: u32,
    num_frag_uniforms: u32,
) -> Result<GraphicsPipeline> {
    let formats = device.get_shader_formats();
    let (vert_code, vert_fmt, vert_entry) = select_shader(&QUAD_VERT, formats)?;
    let (frag_code, frag_fmt, frag_entry) = select_shader(frag, formats)?;

    let vert = device
        .create_shader()
        .with_code(vert_fmt, vert_code, ShaderStage::Vertex)
        .with_entrypoint(vert_entry)
        .build()
        .map_err(|e| anyhow!("build vertex shader: {e}"))?;
    let frag = device
        .create_shader()
        .with_code(frag_fmt, frag_code, ShaderStage::Fragment)
        .with_samplers(num_samplers)
        .with_uniform_buffers(num_frag_uniforms)
        .with_entrypoint(frag_entry)
        .build()
        .map_err(|e| anyhow!("build fragment shader: {e}"))?;

    let swapchain_format = device.get_swapchain_texture_format(window);

    let blend = ColorTargetBlendState::new()
        .with_enable_blend(true)
        .with_color_blend_op(BlendOp::Add)
        .with_alpha_blend_op(BlendOp::Add)
        .with_src_color_blendfactor(BlendFactor::SrcAlpha)
        .with_dst_color_blendfactor(BlendFactor::OneMinusSrcAlpha)
        .with_src_alpha_blendfactor(BlendFactor::One)
        .with_dst_alpha_blendfactor(BlendFactor::OneMinusSrcAlpha);

    let pipeline = device
        .create_graphics_pipeline()
        .with_primitive_type(PrimitiveType::TriangleList)
        .with_vertex_shader(&vert)
        .with_fragment_shader(&frag)
        .with_vertex_input_state(
            VertexInputState::new()
                .with_vertex_buffer_descriptions(&[VertexBufferDescription::new()
                    .with_slot(0)
                    .with_pitch(std::mem::size_of::<Vertex>() as u32)
                    .with_input_rate(VertexInputRate::Vertex)
                    .with_instance_step_rate(0)])
                .with_vertex_attributes(&[
                    VertexAttribute::new()
                        .with_location(0)
                        .with_buffer_slot(0)
                        .with_format(VertexElementFormat::Float2)
                        .with_offset(0),
                    VertexAttribute::new()
                        .with_location(1)
                        .with_buffer_slot(0)
                        .with_format(VertexElementFormat::Float2)
                        .with_offset(8),
                    VertexAttribute::new()
                        .with_location(2)
                        .with_buffer_slot(0)
                        .with_format(VertexElementFormat::Float4)
                        .with_offset(16),
                ]),
        )
        .with_target_info(
            GraphicsPipelineTargetInfo::new().with_color_target_descriptions(&[
                ColorTargetDescription::new()
                    .with_format(swapchain_format)
                    .with_blend_state(blend),
            ]),
        )
        .build()
        .map_err(|e| anyhow!("build graphics pipeline: {e}"))?;

    Ok(pipeline)
}

// --- Text rasterisation (fontdue) -------------------------------------------

fn load_system_font() -> Option<fontdue::Font> {
    // Search common per-platform locations. Bundling a font is a possible future
    // change; for now we degrade gracefully (no text) if none is found.
    const CANDIDATES: &[&str] = &[
        // Linux
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        // Windows
        "C:/Windows/Fonts/segoeui.ttf",
        "C:/Windows/Fonts/arial.ttf",
        // macOS
        "/System/Library/Fonts/SFNS.ttf",
        "/Library/Fonts/Arial.ttf",
    ];
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(font) =
                fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
            {
                log::info!("Loaded UI font from {path}");
                return Some(font);
            }
        }
    }
    log::warn!("No system font found; text overlays will not render.");
    None
}

/// Compute (width, height, baseline) in pixels for a single line.
fn measure_line(font: &fontdue::Font, text: &str, px: f32) -> (u32, u32, f32) {
    let lm = font.horizontal_line_metrics(px);
    let (ascent, descent) = match lm {
        Some(m) => (m.ascent, m.descent),
        None => (px, 0.0),
    };
    let mut width = 0.0f32;
    for ch in text.chars() {
        // Control chars (newline, tab, …) have no glyph — skip them so they don't
        // render as `.notdef` tofu boxes. Callers lay out multiple lines.
        if ch.is_control() {
            continue;
        }
        let m = font.metrics(ch, px);
        width += m.advance_width;
    }
    let h = (ascent - descent).ceil().max(1.0);
    ((width.ceil() as u32).max(1), h as u32, ascent)
}

/// Rasterise a single line of text into an RGBA buffer (white with per-pixel
/// alpha coverage). Returns `None` if the line is degenerate.
fn rasterize_line(font: &fontdue::Font, text: &str, px: f32) -> Option<PixelBuf> {
    let (w, h, baseline) = measure_line(font, text, px);
    if w == 0 || h == 0 {
        return None;
    }
    let mut rgba = vec![0u8; (w * h * 4) as usize];

    let mut pen_x = 0.0f32;
    for ch in text.chars() {
        if ch.is_control() {
            continue;
        }
        let (m, bitmap) = font.rasterize(ch, px);
        let gx = (pen_x + m.xmin as f32).round() as i32;
        // Top of the glyph bitmap relative to the baseline.
        let gy = (baseline - (m.height as f32 + m.ymin as f32)).round() as i32;

        for by in 0..m.height {
            let py = gy + by as i32;
            if py < 0 || py >= h as i32 {
                continue;
            }
            for bx in 0..m.width {
                let pxp = gx + bx as i32;
                if pxp < 0 || pxp >= w as i32 {
                    continue;
                }
                let cov = bitmap[by * m.width + bx];
                if cov == 0 {
                    continue;
                }
                let idx = ((py as u32 * w + pxp as u32) * 4) as usize;
                // White, coverage as alpha. Keep the max so overlapping AA edges
                // don't darken.
                rgba[idx] = 255;
                rgba[idx + 1] = 255;
                rgba[idx + 2] = 255;
                rgba[idx + 3] = rgba[idx + 3].max(cov);
            }
        }
        pen_x += m.advance_width;
    }

    Some(PixelBuf::new(w, h, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Newlines/control chars must not be laid out as glyphs (the `.notdef`
    /// "tofu" box) — they should contribute no width. Skips if no system font.
    #[test]
    fn control_chars_have_no_glyph_width() {
        let Some(font) = load_system_font() else {
            return;
        };
        let (w_plain, _, _) = measure_line(&font, "ab", 24.0);
        let (w_newline, _, _) = measure_line(&font, "a\nb", 24.0);
        let (w_tab, _, _) = measure_line(&font, "a\tb", 24.0);
        assert_eq!(w_plain, w_newline, "newline should add no width");
        assert_eq!(w_plain, w_tab, "tab should add no width");
    }
}
