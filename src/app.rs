// --- Main Application State ---
//
// Owns all viewer/player state and drives it from the SDL3 main loop via three
// entry points: `handle_event` (input), `update` (per-frame state advance), and
// `render` (draw). Rendering goes through the SDL_GPU `Renderer`; there is no
// egui involvement.

use arboard::{Clipboard, ImageData};
use sdl3::event::Event;
use sdl3::keyboard::{Keycode, Mod};
use sdl3::mouse::MouseButton;
use std::{
    borrow::Cow,
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

use crate::cache::{load_preload_cache, preload_cache_path};
use crate::config::KeyBindings;
use crate::decode::{downscale_pixel_buf, is_video_file, scan_supported_images, to_pixel_buf};
use crate::geom::{Rect, Vec2};
use crate::gpu::{gray, rgba8, Renderer, TextAlign, VideoColorParams, WHITE};
use crate::thumbnail::load_embedded_thumbnail;
use crate::types::{
    Animation, AnimationFrame, DisplayableImage, FullResRequest, FullResWorker, LoadedImage,
    MemoryGate, PixelBuf, PreloadState,
};
use crate::video_state::VideoState;
use crate::workers::{spawn_full_res_worker, spawn_preload_workers};

const TILE_SIZE: usize = 1024; // Use tiles of 1024x1024 pixels for the detail view
/// Largest single texture we upload; bigger images are tiled. A conservative
/// value that every GPU supports (real limits are higher, just more tiling).
const MAX_TEXTURE_SIDE: usize = 2048;
/// Maximum time we wait for a full-res decode before assuming the worker is stuck
/// (slow/hung decoder, bad file). After this we respawn the worker and unblock
/// the bulk preload so the app doesn't sit there silently forever.
const FULL_RES_WATCHDOG: Duration = Duration::from_secs(20);

/// Fit `content` (in pixels) into `area`, preserving aspect ratio and centering.
/// Used to letterbox/pillarbox video frames in the central panel.
fn fit_centered(content: Vec2, area: Rect) -> Rect {
    if content.x <= 0.0 || content.y <= 0.0 {
        return area;
    }
    let aspect = content.x / content.y;
    let area_aspect = area.width() / area.height();
    let mut size = area.size();
    if aspect > area_aspect {
        size.y = size.x / aspect;
    } else {
        size.x = size.y * aspect;
    }
    let offset = (area.size() - size) * 0.5;
    Rect::from_min_size(area.min + offset, size)
}

/// Format a duration in seconds as `M:SS` (or `H:MM:SS` past an hour).
fn format_time(secs: f64) -> String {
    let s = secs.max(0.0) as i64;
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

/// Draw a subtitle line centered near the bottom of `area`, with a black
/// outline so it stays legible over any frame.
fn draw_subtitle(r: &mut Renderer, area: Rect, text: &str) {
    let size = (area.height() * 0.045).clamp(18.0, 40.0);
    let line_h = size * 1.25;
    // Subtitles are often multi-line (`\n`-separated); stack the lines so the last
    // sits at the usual bottom anchor and earlier lines go above it.
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let bottom_y = area.max().y - area.height() * 0.05 - size;
    let n = lines.len();
    for (i, line) in lines.iter().enumerate() {
        let y = bottom_y - (n - 1 - i) as f32 * line_h;
        r.draw_text_outlined(line, size, Vec2::new(area.center().x, y), TextAlign::Center, WHITE);
    }
}

/// Draw a transient on-screen status message in the top-left of `area`.
fn draw_osd(r: &mut Renderer, area: Rect, text: &str) {
    let size = 18.0;
    let pos = area.min + Vec2::new(16.0, 16.0);
    let ts = r.text_size(text, size);
    let bg = Rect::from_min_size(pos - Vec2::new(6.0, 6.0), ts + Vec2::new(12.0, 12.0));
    r.fill_rect(bg, rgba8(0, 0, 0, 160));
    r.draw_text(text, size, pos, TextAlign::Left, WHITE);
}

/// Draw a graphical seek/progress bar near the bottom of `area`, with the
/// current position and total duration.
/// Geometry of the seek bar within `area`, shared by drawing and mouse hit-testing.
struct SeekBarGeom {
    left: f32,
    right: f32,
    track_y: f32,
    track_h: f32,
    /// Generous clickable band around the thin track.
    hit: Rect,
}

fn seek_bar_geom(area: Rect) -> Option<SeekBarGeom> {
    let margin = (area.width() * 0.05).clamp(16.0, 120.0);
    let left = area.min.x + margin;
    let right = area.max().x - margin;
    if right <= left {
        return None;
    }
    let track_y = area.max().y - 28.0;
    let track_h = 6.0;
    let hit = Rect::from_min_max(
        Vec2::new(left - 8.0, track_y - 12.0),
        Vec2::new(right + 8.0, track_y + track_h + 14.0),
    );
    Some(SeekBarGeom { left, right, track_y, track_h, hit })
}

/// Fraction (0..1) along the seek bar for a given screen x.
fn seek_bar_frac_at(g: &SeekBarGeom, x: f32) -> f32 {
    ((x - g.left) / (g.right - g.left)).clamp(0.0, 1.0)
}

/// Draw the seek/progress bar. While the user is scrubbing, `scrub_frac` overrides
/// the marker position (a live preview) and the time label shows the scrub target.
fn draw_seek_bar(
    r: &mut Renderer,
    area: Rect,
    pos_secs: f64,
    dur_secs: Option<f64>,
    scrub_frac: Option<f32>,
    looping: bool,
) {
    let Some(g) = seek_bar_geom(area) else {
        return;
    };

    // The fraction (and time) we display: the scrub target if dragging, else the
    // live playback position.
    let display_secs = match (scrub_frac, dur_secs) {
        (Some(f), Some(d)) => f as f64 * d,
        _ => pos_secs,
    };
    let label = match dur_secs {
        Some(d) => format!("{} / {}", format_time(display_secs), format_time(d)),
        None => format_time(display_secs),
    };
    let label_y = g.track_y - g.track_h - 18.0;
    r.draw_text(&label, 14.0, Vec2::new(g.right, label_y), TextAlign::Right, WHITE);
    // Loop indicator, left-aligned opposite the time readout, while loop mode is
    // on. Plain ASCII so it can't render as tofu on systems whose font lacks a
    // loop glyph (the font is system-provided, not bundled).
    if looping {
        r.draw_text("Loop", 14.0, Vec2::new(g.left, label_y), TextAlign::Left, WHITE);
    }

    let frac = match scrub_frac {
        Some(f) => f,
        None => match dur_secs.filter(|d| *d > 0.0) {
            Some(d) => (pos_secs / d).clamp(0.0, 1.0) as f32,
            None => return, // unknown duration and not scrubbing: label only
        },
    };

    // Background track, elapsed fill, then a playhead knob.
    r.fill_rect(Rect::xywh(g.left, g.track_y, g.right - g.left, g.track_h), rgba8(0, 0, 0, 160));
    let fill_w = (g.right - g.left) * frac;
    r.fill_rect(Rect::xywh(g.left, g.track_y, fill_w, g.track_h), gray(230));
    // The knob grows a little while scrubbing so it's easy to see where you'll land.
    let knob = if scrub_frac.is_some() { g.track_h * 3.0 } else { g.track_h * 2.0 };
    r.fill_rect(
        Rect::xywh(g.left + fill_w - knob / 2.0, g.track_y + g.track_h / 2.0 - knob / 2.0, knob, knob),
        WHITE,
    );
}

// Context-menu geometry.
const MENU_W: f32 = 240.0;
const MENU_ROW_H: f32 = 30.0;
const MENU_PAD: f32 = 5.0;
const MENU_ITEMS: usize = 3;

/// Compute the context-menu panel rect and its per-row rects, clamped so the
/// menu stays fully on-screen. Shared by `handle_event` (hit-testing) and
/// `render` (drawing) so the two never disagree.
fn context_menu_layout(anchor: Vec2, area: Rect) -> (Rect, [Rect; MENU_ITEMS]) {
    let h = MENU_ROW_H * MENU_ITEMS as f32 + MENU_PAD * 2.0;
    let x = anchor.x.min(area.max().x - MENU_W).max(area.min.x);
    let y = anchor.y.min(area.max().y - h).max(area.min.y);
    let panel = Rect::xywh(x, y, MENU_W, h);
    let mut rows = [Rect::xywh(0.0, 0.0, 0.0, 0.0); MENU_ITEMS];
    for (i, row) in rows.iter_mut().enumerate() {
        *row = Rect::xywh(
            x + MENU_PAD,
            y + MENU_PAD + i as f32 * MENU_ROW_H,
            MENU_W - MENU_PAD * 2.0,
            MENU_ROW_H,
        );
    }
    (panel, rows)
}

/// Compute the delete-dialog panel and its (Cancel, Delete) button rects.
fn delete_dialog_layout(area: Rect) -> (Rect, Rect, Rect) {
    let panel_w = (area.width() * 0.6).clamp(320.0, 720.0);
    let panel_h = 150.0;
    let panel = Rect::xywh(
        area.center().x - panel_w / 2.0,
        area.center().y - panel_h / 2.0,
        panel_w,
        panel_h,
    );
    let btn_w = 130.0;
    let btn_h = 36.0;
    let by = panel.max().y - btn_h - 18.0;
    let gap = 24.0;
    let cancel = Rect::xywh(panel.center().x - btn_w - gap / 2.0, by, btn_w, btn_h);
    let delete = Rect::xywh(panel.center().x + gap / 2.0, by, btn_w, btn_h);
    (panel, cancel, delete)
}

pub struct ImageViewerApp {
    image: Option<DisplayableImage>,
    /// Active video playback state. Mutually exclusive with `image`.
    video: Option<VideoState>,
    image_files: Vec<PathBuf>,
    current_index: usize,
    image_order: Vec<usize>,
    zoom: f32,
    offset: Vec2,
    velocity: Vec2,
    is_scaled_to_fit: bool,
    is_fullscreen: bool,
    is_randomized: bool,
    show_delete_confirmation: bool,
    last_error: Option<String>,
    clipboard: Option<Clipboard>,
    full_res_pending: bool,
    full_res_pending_since: Option<Instant>,
    full_res_worker: Option<FullResWorker>,
    preload_state: Option<Arc<PreloadState>>,
    memory_gate: Arc<MemoryGate>,
    /// Configurable key bindings for video seeking and file browsing.
    keybindings: KeyBindings,

    // --- input state (event-driven) ---
    mouse_pos: Vec2,
    dragging: bool,
    /// Set when a drag/zoom happened this frame; suppresses the bounce physics
    /// for that frame (mirrors the old `is_interacting`).
    interacted: bool,
    /// When `Some`, the right-click context menu is open, anchored at this point.
    context_menu: Option<Vec2>,
    /// True while dragging the seek-bar marker; `scrub_frac` is the live target
    /// fraction, committed as a seek on mouse-up.
    scrubbing: bool,
    scrub_frac: f32,
    should_quit: bool,
}

impl ImageViewerApp {
    pub fn new(path: Option<PathBuf>, initial_fullscreen: bool, renderer: &Renderer) -> Self {
        let memory_gate = Arc::new(MemoryGate::new());
        let full_res_worker = Some(spawn_full_res_worker(memory_gate.clone()));
        let keybindings = crate::config::Config::load().keybindings;
        let mut app = Self {
            image: None,
            video: None,
            image_files: Vec::new(),
            current_index: 0,
            image_order: Vec::new(),
            zoom: 1.0,
            offset: Vec2::ZERO,
            velocity: Vec2::ZERO,
            is_scaled_to_fit: true,
            is_fullscreen: initial_fullscreen,
            is_randomized: false,
            show_delete_confirmation: false,
            last_error: None,
            clipboard: Clipboard::new().ok(),
            full_res_pending: false,
            full_res_pending_since: None,
            full_res_worker,
            preload_state: None,
            memory_gate,
            keybindings,
            mouse_pos: Vec2::ZERO,
            dragging: false,
            interacted: false,
            context_menu: None,
            scrubbing: false,
            scrub_frac: 0.0,
            should_quit: false,
        };
        if let Some(path) = path {
            app.gather_images_from_directory(&path);
            if !app.image_files.is_empty() {
                app.load_image_at_index(app.current_index, renderer);
                app.start_bulk_preload();
            } else {
                app.last_error = Some(format!(
                    "No supported images found in directory of '{}'",
                    path.display()
                ));
            }
        } else {
            app.last_error = Some("No image file specified.".to_string());
        }
        app
    }

    fn load_image_at_index(&mut self, index: usize, renderer: &Renderer) {
        self.current_index = index;
        let path = self.image_files[self.image_order[self.current_index]].clone();
        log::info!("Loading image: {}", path.display());
        let start_time = Instant::now();

        self.is_scaled_to_fit = true;
        self.velocity = Vec2::ZERO;
        self.full_res_pending = false;
        self.full_res_pending_since = None;

        // Video files bypass the image decode/cache/tile pipeline entirely.
        if is_video_file(&path) {
            self.video = None;
            self.image = None;
            match VideoState::open(&path) {
                Ok(state) => {
                    self.video = Some(state);
                    self.last_error = None;
                }
                Err(e) => {
                    self.last_error =
                        Some(format!("Failed to open video '{}': {e}", path.display()));
                }
            }
            return;
        }

        // Switching to an image: stop any video that was playing.
        self.video = None;

        if let Some(LoadedImage::Static(preview)) = load_preload_cache(&path) {
            log::info!(
                "Loaded preload-cache preview for '{}' in {:.2?}",
                path.display(),
                start_time.elapsed()
            );
            self.display_loaded_image(preview, renderer);
            self.start_full_res_load(path, renderer);
        } else if let Some(thumb) = load_embedded_thumbnail(&path) {
            log::info!(
                "Loaded embedded thumbnail for '{}' in {:.2?}",
                path.display(),
                start_time.elapsed()
            );
            self.display_loaded_image(to_pixel_buf(thumb), renderer);
            self.start_full_res_load(path, renderer);
        } else {
            // No preview available; route the decode through the worker and show a
            // "Loading…" placeholder until the reply arrives.
            self.image = None;
            self.last_error = None;
            self.start_full_res_load(path, renderer);
        }
    }

    fn display_loaded_image(&mut self, image: PixelBuf, renderer: &Renderer) {
        let needs_tiling =
            image.width() > MAX_TEXTURE_SIDE || image.height() > MAX_TEXTURE_SIDE;

        let preview_image = if needs_tiling {
            downscale_pixel_buf(&image, MAX_TEXTURE_SIDE)
        } else {
            image.clone()
        };

        let preview_texture = match renderer.upload_texture(&preview_image) {
            Ok(t) => t,
            Err(e) => {
                self.last_error = Some(format!("Failed to upload image: {e}"));
                return;
            }
        };

        self.image = Some(DisplayableImage {
            full_res_image: image,
            preview_texture,
            tile_cache: HashMap::new(),
            needs_tiling,
            animation: None,
        });
        self.last_error = None;
    }

    fn display_animated_image(&mut self, frames: Vec<AnimationFrame>, renderer: &Renderer) {
        let first_frame = frames[0].image.clone();
        let preview_texture = match renderer.upload_texture(&first_frame) {
            Ok(t) => t,
            Err(e) => {
                self.last_error = Some(format!("Failed to upload image: {e}"));
                return;
            }
        };

        self.image = Some(DisplayableImage {
            full_res_image: first_frame,
            preview_texture,
            tile_cache: HashMap::new(),
            needs_tiling: false,
            animation: Some(Animation {
                frames,
                current: 0,
                frame_started: Instant::now(),
            }),
        });
        self.last_error = None;
    }

    fn start_full_res_load(&mut self, path: PathBuf, _renderer: &Renderer) {
        if let Some(state) = &self.preload_state {
            state.pause.store(true, Ordering::Relaxed);
        }
        let preview_width = self
            .image
            .as_ref()
            .map(|i| i.full_res_image.width() as u32)
            .unwrap_or(0);
        let request = FullResRequest { path: path.clone(), preview_width };
        let send_result = self.full_res_worker.as_ref().map(|w| w.tx.send(request));
        // If the worker channel is gone (e.g. it panicked out of catch_unwind),
        // respawn it so subsequent navigations still get full-res loads.
        if !matches!(send_result, Some(Ok(()))) {
            log::warn!("Full-res worker unavailable; respawning.");
            let worker = spawn_full_res_worker(self.memory_gate.clone());
            let _ = worker.tx.send(FullResRequest { path, preview_width });
            self.full_res_worker = Some(worker);
        }
        self.full_res_pending = true;
        self.full_res_pending_since = Some(Instant::now());
    }

    fn check_pending_load(&mut self, renderer: &Renderer) {
        // Watchdog: if a full-res decode hasn't returned for too long, the worker
        // is likely stuck on a slow/bad file. Drop it so the next nav respawns one.
        if self.full_res_pending {
            if let Some(since) = self.full_res_pending_since {
                if since.elapsed() > FULL_RES_WATCHDOG {
                    log::warn!(
                        "Full-res worker stuck for {:.1?}; respawning on next navigation.",
                        since.elapsed()
                    );
                    self.full_res_worker = None;
                    self.full_res_pending = false;
                    self.full_res_pending_since = None;
                    if let Some(state) = &self.preload_state {
                        state.pause.store(false, Ordering::Relaxed);
                    }
                }
            }
        }

        let Some(worker) = self.full_res_worker.as_ref() else { return };
        loop {
            let reply = match worker.rx.try_recv() {
                Ok(r) => r,
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.full_res_worker = None;
                    self.full_res_pending = false;
                    self.full_res_pending_since = None;
                    if let Some(state) = &self.preload_state {
                        state.pause.store(false, Ordering::Relaxed);
                    }
                    return;
                }
            };

            let current_path = self
                .image_files
                .get(self.image_order.get(self.current_index).copied().unwrap_or(usize::MAX))
                .cloned();
            if current_path.as_ref() != Some(&reply.path) {
                log::debug!("Discarding stale full-res reply for {}", reply.path.display());
                continue;
            }

            if !reply.is_preview {
                self.full_res_pending = false;
                self.full_res_pending_since = None;
                if let Some(state) = &self.preload_state {
                    state.pause.store(false, Ordering::Relaxed);
                }
            }
            match reply.result {
                Ok(loaded) => {
                    let new_width = match &loaded {
                        LoadedImage::Static(img) => img.width() as f32,
                        LoadedImage::Animated(frames) => {
                            frames.first().map(|f| f.image.width()).unwrap_or(0) as f32
                        }
                    };
                    let preview_width = reply.preview_width as f32;
                    if preview_width > 0.0 && new_width > 0.0 && !self.is_scaled_to_fit {
                        // Preserve the user's current view across the preview→full swap.
                        self.zoom *= preview_width / new_width;
                    }
                    match loaded {
                        LoadedImage::Static(full_res) => {
                            self.display_loaded_image(full_res, renderer)
                        }
                        LoadedImage::Animated(frames) => {
                            self.display_animated_image(frames, renderer)
                        }
                    }
                    if reply.is_preview {
                        log::info!("Showed fast preview for: {}", reply.path.display());
                    } else {
                        log::info!("Swapped in full-res image: {}", reply.path.display());
                    }
                }
                Err(e) => {
                    log::error!(
                        "Background full-res load failed for {}: {}",
                        reply.path.display(),
                        e
                    );
                    if self.image.is_none() {
                        self.last_error = Some(e);
                    }
                }
            }
            return;
        }
    }

    fn start_bulk_preload(&mut self) {
        if let Some(state) = self.preload_state.take() {
            state.shutdown.store(true, Ordering::Relaxed);
        }
        let n = self.image_files.len();
        if n <= 1 {
            return;
        }
        // Bounce outward from current_index so images closest to the user — in
        // either direction — are preloaded first.
        let mut paths: Vec<PathBuf> = Vec::with_capacity(n - 1);
        let mut seen = vec![false; n];
        seen[self.current_index] = true;
        let mut d = 1;
        while paths.len() < n - 1 {
            let fwd = (self.current_index + d) % n;
            if !seen[fwd] {
                seen[fwd] = true;
                paths.push(self.image_files[self.image_order[fwd]].clone());
            }
            if paths.len() >= n - 1 {
                break;
            }
            let back = (self.current_index + n - d) % n;
            if !seen[back] {
                seen[back] = true;
                paths.push(self.image_files[self.image_order[back]].clone());
            }
            d += 1;
        }
        let state = Arc::new(PreloadState {
            shutdown: std::sync::atomic::AtomicBool::new(false),
            pause: std::sync::atomic::AtomicBool::new(self.full_res_pending),
            gate: self.memory_gate.clone(),
        });
        self.preload_state = Some(state.clone());
        spawn_preload_workers(state, paths);
    }

    fn shutdown_workers(&mut self) {
        if let Some(state) = &self.preload_state {
            state.shutdown.store(true, Ordering::Relaxed);
        }
        self.full_res_worker = None;
    }

    fn copy_to_clipboard(&mut self) {
        if let (Some(clipboard), Some(image)) = (&mut self.clipboard, &self.image) {
            let image = &image.full_res_image;
            let image_data = ImageData {
                width: image.width(),
                height: image.height(),
                bytes: Cow::from(image.rgba.clone()),
            };
            log::info!("Copying image: {}x{}", image_data.width, image_data.height);
            if let Err(e) = clipboard.set_image(image_data) {
                self.last_error = Some(format!("Failed to copy to clipboard: {}", e));
            } else {
                log::info!("Image copied to clipboard.");
            }
        }
    }

    fn gather_images_from_directory(&mut self, file_path: &Path) {
        let parent_dir = match file_path.parent() {
            Some(p) => p,
            None => {
                self.last_error = Some("Failed to get parent directory.".to_string());
                return;
            }
        };
        let files = scan_supported_images(parent_dir);
        if let Some(index) = files.iter().position(|p| p == file_path) {
            self.current_index = index;
        }
        self.image_files = files;
        self.image_order = (0..self.image_files.len()).collect();
    }

    /// Re-scan the parent directory so files added/removed externally show up the
    /// next time the user navigates.
    fn refresh_directory(&mut self) {
        let parent_dir = match self
            .image_files
            .get(self.image_order.get(self.current_index).copied().unwrap_or(usize::MAX))
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
            .or_else(|| self.image_files.first().and_then(|p| p.parent()).map(|p| p.to_path_buf()))
        {
            Some(p) => p,
            None => return,
        };

        let new_files = scan_supported_images(&parent_dir);
        if new_files == self.image_files {
            return;
        }

        log::info!(
            "Directory contents changed: {} -> {} files",
            self.image_files.len(),
            new_files.len()
        );

        let current_path = self
            .image_files
            .get(self.image_order.get(self.current_index).copied().unwrap_or(usize::MAX))
            .cloned();

        if self.is_randomized {
            let old_path_order: Vec<PathBuf> = self
                .image_order
                .iter()
                .filter_map(|&i| self.image_files.get(i).cloned())
                .collect();
            let mut new_order = Vec::with_capacity(new_files.len());
            let mut seen = vec![false; new_files.len()];
            for path in &old_path_order {
                if let Some(idx) = new_files.iter().position(|p| p == path) {
                    if !seen[idx] {
                        seen[idx] = true;
                        new_order.push(idx);
                    }
                }
            }
            for (idx, was_seen) in seen.iter().enumerate() {
                if !was_seen {
                    new_order.push(idx);
                }
            }
            self.image_order = new_order;
        } else {
            self.image_order = (0..new_files.len()).collect();
        }

        self.image_files = new_files;

        if let Some(cp) = current_path {
            if let Some(file_idx) = self.image_files.iter().position(|p| p == &cp) {
                if let Some(order_idx) = self.image_order.iter().position(|&i| i == file_idx) {
                    self.current_index = order_idx;
                }
            } else if self.current_index >= self.image_order.len() {
                self.current_index = self.image_order.len().saturating_sub(1);
            }
        }

        self.start_bulk_preload();
    }

    fn next_image(&mut self, renderer: &Renderer) {
        self.refresh_directory();
        if !self.image_files.is_empty() {
            self.load_image_at_index((self.current_index + 1) % self.image_files.len(), renderer);
        }
    }

    fn prev_image(&mut self, renderer: &Renderer) {
        self.refresh_directory();
        if !self.image_files.is_empty() {
            self.load_image_at_index(
                (self.current_index + self.image_files.len() - 1) % self.image_files.len(),
                renderer,
            );
        }
    }

    fn first_image(&mut self, renderer: &Renderer) {
        self.refresh_directory();
        if !self.image_files.is_empty() {
            self.load_image_at_index(0, renderer);
        }
    }

    fn last_image(&mut self, renderer: &Renderer) {
        self.refresh_directory();
        if !self.image_files.is_empty() {
            self.load_image_at_index(self.image_files.len() - 1, renderer);
        }
    }

    /// Delete the current file (after confirmation) and advance.
    fn perform_delete(&mut self, renderer: &Renderer) {
        self.show_delete_confirmation = false;
        let Some(path) = self.image_files.get(self.image_order[self.current_index]).cloned() else {
            return;
        };
        // Compute the cache path before deleting — the hash needs the file's size/mtime.
        let cache_path = preload_cache_path(&path);
        if let Err(e) = fs::remove_file(&path) {
            self.last_error = Some(format!("Failed to delete file: {}", e));
            return;
        }
        log::info!("Deleted file: {}", path.display());
        if cache_path.exists() {
            if let Err(e) = fs::remove_file(&cache_path) {
                log::warn!("Failed to delete preload cache {}: {}", cache_path.display(), e);
            }
        }
        let removed_order_index = self.image_order.remove(self.current_index);
        self.image_files.remove(removed_order_index);
        for order_idx in self.image_order.iter_mut() {
            if *order_idx > removed_order_index {
                *order_idx -= 1;
            }
        }
        if self.image_files.is_empty() {
            self.should_quit = true;
        } else {
            self.current_index %= self.image_files.len();
            self.load_image_at_index(self.current_index, renderer);
        }
    }

    /// If the seek bar is currently visible and `p` lands on its clickable band,
    /// begin scrubbing. Returns whether a scrub started (so the left-click handler
    /// skips the image-pan path).
    fn try_start_scrub(&mut self, p: Vec2, area: Rect) -> bool {
        let visible = self
            .video
            .as_ref()
            .map(|v| v.controls_visible() && v.duration_secs().is_some())
            .unwrap_or(false);
        if !visible {
            return false;
        }
        let Some(g) = seek_bar_geom(area) else {
            return false;
        };
        if !g.hit.contains(p) {
            return false;
        }
        self.scrubbing = true;
        self.scrub_frac = seek_bar_frac_at(&g, p.x);
        if let Some(v) = &mut self.video {
            v.bump_controls();
        }
        true
    }

    // --- Event handling ------------------------------------------------------

    pub fn handle_event(&mut self, event: &Event, renderer: &mut Renderer) {
        match event {
            Event::Quit { .. } => self.should_quit = true,
            Event::KeyDown { keycode: Some(kc), keymod, repeat: false, .. } => {
                self.on_key(*kc, *keymod, renderer);
            }
            Event::MouseButtonDown { mouse_btn: MouseButton::Left, x, y, .. } => {
                let p = Vec2::new(*x, *y);
                self.mouse_pos = p;
                let area = Rect::from_min_size(Vec2::ZERO, renderer.drawable_size());
                if self.show_delete_confirmation {
                    // Hit-test the dialog buttons; clicks elsewhere keep it open.
                    let (_, cancel, delete) = delete_dialog_layout(area);
                    if delete.contains(p) {
                        self.perform_delete(renderer);
                    } else if cancel.contains(p) {
                        self.show_delete_confirmation = false;
                    }
                } else if let Some(anchor) = self.context_menu {
                    // Hit-test the menu rows; a click outside dismisses it.
                    let (panel, rows) = context_menu_layout(anchor, area);
                    if rows[0].contains(p) {
                        self.is_fullscreen = !self.is_fullscreen;
                        self.context_menu = None;
                    } else if rows[1].contains(p) {
                        self.is_scaled_to_fit = !self.is_scaled_to_fit;
                        self.context_menu = None;
                    } else if rows[2].contains(p) {
                        self.toggle_random_order();
                        self.context_menu = None;
                    } else if !panel.contains(p) {
                        self.context_menu = None;
                    }
                } else if self.try_start_scrub(p, area) {
                    // Grabbed the seek-bar marker; scrubbing handled on motion/up.
                } else {
                    self.dragging = true;
                }
            }
            Event::MouseButtonDown { mouse_btn: MouseButton::Right, x, y, .. } => {
                let p = Vec2::new(*x, *y);
                self.mouse_pos = p;
                // Open the context menu (only over image/video, not a modal).
                if !self.show_delete_confirmation {
                    self.context_menu = Some(p);
                }
            }
            Event::MouseButtonUp { mouse_btn: MouseButton::Left, .. } => {
                if self.scrubbing {
                    // Commit the seek to the marker's final position.
                    self.scrubbing = false;
                    let frac = self.scrub_frac;
                    if let Some(v) = &mut self.video {
                        v.seek_to_fraction(frac as f64);
                    }
                }
                self.dragging = false;
            }
            Event::MouseMotion { x, y, xrel, yrel, .. } => {
                let p = Vec2::new(*x, *y);
                self.mouse_pos = p;
                // Moving the mouse over a video re-shows the HUD, so the seek bar
                // is reachable (standard media-player behaviour).
                if let Some(v) = &mut self.video {
                    v.bump_controls();
                }
                if self.scrubbing {
                    let area = Rect::from_min_size(Vec2::ZERO, renderer.drawable_size());
                    if let Some(g) = seek_bar_geom(area) {
                        self.scrub_frac = seek_bar_frac_at(&g, p.x);
                    }
                } else if self.dragging && self.image.is_some() {
                    let delta = Vec2::new(*xrel, *yrel);
                    self.offset += delta;
                    // Smooth momentum over the recent gesture.
                    self.velocity = self.velocity * 0.4 + delta * 0.6;
                    self.is_scaled_to_fit = false;
                    self.interacted = true;
                }
            }
            Event::MouseWheel { y, mouse_x, mouse_y, .. } => {
                if self.image.is_some() && *y != 0.0 {
                    let cursor = Vec2::new(*mouse_x, *mouse_y);
                    let old_zoom = self.zoom;
                    // Scale the wheel delta to roughly match the old feel.
                    let scroll = *y * 40.0;
                    let zoom_delta = (scroll / 200.0) * self.zoom;
                    self.zoom = (self.zoom + zoom_delta).max(0.001);
                    let image_coords = (cursor - self.offset) / old_zoom;
                    self.offset -= image_coords * (self.zoom - old_zoom);
                    self.is_scaled_to_fit = false;
                    self.velocity = Vec2::ZERO;
                    self.interacted = true;
                }
            }
            _ => {}
        }
    }

    fn on_key(&mut self, kc: Keycode, keymod: Mod, renderer: &Renderer) {
        let ctrl = keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD);

        // Clipboard copy (Ctrl+C).
        if ctrl && kc == Keycode::C {
            self.copy_to_clipboard();
            return;
        }

        // Delete confirmation modal swallows most keys.
        if self.show_delete_confirmation {
            match kc {
                Keycode::Return => self.perform_delete(renderer),
                Keycode::Escape => self.show_delete_confirmation = false,
                _ => {}
            }
            return;
        }

        let (seek_back, seek_fwd) = self.keybindings.video_seek.keys();
        let (browse_prev, browse_next) = self.keybindings.file_browse.keys();

        if self.video.is_some() {
            match kc {
                Keycode::Space => {
                    let shift = keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD);
                    if let Some(v) = &mut self.video {
                        if shift {
                            // Shift+Space toggles loop mode.
                            v.toggle_loop();
                        } else if v.is_finished() {
                            // Space after the video has played out restarts it.
                            v.restart();
                        } else {
                            v.toggle_pause();
                        }
                    }
                }
                Keycode::A => {
                    if let Some(v) = &mut self.video {
                        v.cycle_audio_track();
                    }
                }
                Keycode::S => {
                    if let Some(v) = &mut self.video {
                        v.cycle_subtitle_track();
                    }
                }
                _ => {}
            }
            let seek_step = if ctrl { 60.0 } else { 5.0 };
            if kc == seek_fwd {
                if let Some(v) = &mut self.video {
                    v.seek_relative(seek_step);
                }
            } else if kc == seek_back {
                if let Some(v) = &mut self.video {
                    v.seek_relative(-seek_step);
                }
            }
            // File browsing on keys not already used for seeking.
            if kc == browse_next && browse_next != seek_back && browse_next != seek_fwd {
                self.next_image(renderer);
            } else if kc == browse_prev && browse_prev != seek_back && browse_prev != seek_fwd {
                self.prev_image(renderer);
            }
        } else {
            if kc == browse_next {
                self.next_image(renderer);
            } else if kc == browse_prev {
                self.prev_image(renderer);
            }
        }

        match kc {
            Keycode::Home => self.first_image(renderer),
            Keycode::End => self.last_image(renderer),
            // Escape closes the context menu first, otherwise quits.
            Keycode::Escape => {
                if self.context_menu.take().is_none() {
                    self.should_quit = true;
                }
            }
            Keycode::F => {
                self.is_fullscreen = !self.is_fullscreen;
            }
            Keycode::Return => self.is_scaled_to_fit = !self.is_scaled_to_fit,
            Keycode::Delete => self.show_delete_confirmation = true,
            _ => {}
        }
    }

    /// Toggle randomized traversal order, preserving the currently-shown image.
    fn toggle_random_order(&mut self) {
        self.is_randomized = !self.is_randomized;
        if self.image_files.is_empty() {
            return;
        }
        let current = self.image_order[self.current_index];
        if self.is_randomized {
            use rand::seq::SliceRandom;
            let mut rng = rand::rng();
            self.image_order.shuffle(&mut rng);
        } else {
            self.image_order = (0..self.image_files.len()).collect();
        }
        if let Some(pos) = self.image_order.iter().position(|&i| i == current) {
            self.current_index = pos;
        }
    }

    // --- Per-frame update ----------------------------------------------------

    pub fn update(&mut self, renderer: &mut Renderer) {
        self.check_pending_load(renderer);

        // Engage HDR passthrough when HDR video plays on an HDR-capable display,
        // and drop back to SDR tone-mapping otherwise (incl. moving to an SDR
        // monitor). Re-evaluated every frame; reconfiguration is rare.
        let content_is_hdr = self
            .video
            .as_ref()
            .and_then(|v| v.video_color())
            .map(|c| c.is_hdr())
            .unwrap_or(false);
        renderer.update_hdr_output(content_is_hdr);

        let area = Rect::from_min_size(Vec2::ZERO, renderer.drawable_size());

        if let Some(video) = &mut self.video {
            video.tick(renderer);
        } else if let Some(image) = &mut self.image {
            // Advance animation playback (GIFs).
            if let Some(anim) = &mut image.animation {
                if anim.frames.len() > 1 {
                    let mut changed = false;
                    let mut steps = 0;
                    while steps < anim.frames.len()
                        && anim.frame_started.elapsed() >= anim.frames[anim.current].delay
                    {
                        anim.frame_started += anim.frames[anim.current].delay;
                        anim.current = (anim.current + 1) % anim.frames.len();
                        changed = true;
                        steps += 1;
                    }
                    if steps == anim.frames.len() {
                        anim.frame_started = Instant::now();
                    }
                    if changed {
                        let frame_image = anim.frames[anim.current].image.clone();
                        if let Ok(tex) = renderer.upload_texture(&frame_image) {
                            image.preview_texture = tex;
                        }
                        image.full_res_image = frame_image;
                    }
                }
            }

            // Zoom/pan physics (only for stills/animations, not video).
            let full_res_size =
                Vec2::new(image.full_res_image.width() as f32, image.full_res_image.height() as f32);

            if self.is_scaled_to_fit {
                let aspect_ratio = full_res_size.x / full_res_size.y;
                let available_aspect = area.width() / area.height();
                let mut fit_size = area.size();
                if aspect_ratio > available_aspect {
                    fit_size.y = fit_size.x / aspect_ratio;
                } else {
                    fit_size.x = fit_size.y * aspect_ratio;
                }
                self.zoom = fit_size.x / full_res_size.x;
                self.offset = (area.size() - fit_size) * 0.5;
                self.velocity = Vec2::ZERO;
            } else {
                if !self.dragging {
                    self.offset += self.velocity;
                }
                let interacting = self.dragging || self.interacted;
                if !interacting {
                    let screen_size = area.size();
                    let scaled = full_res_size * self.zoom;
                    let friction = 0.92;
                    let tension = 0.06;
                    let damping = 0.65;
                    let handle_axis =
                        |offset: &mut f32, velocity: &mut f32, view_dim: f32, img_dim: f32| {
                            let target_pos;
                            let is_out_of_bounds;
                            if img_dim <= view_dim {
                                target_pos = (view_dim - img_dim) / 2.0;
                                is_out_of_bounds = (*offset - target_pos).abs() > 0.5;
                            } else {
                                let min = view_dim - img_dim;
                                let max = 0.0;
                                if *offset > max {
                                    target_pos = max;
                                    is_out_of_bounds = true;
                                } else if *offset < min {
                                    target_pos = min;
                                    is_out_of_bounds = true;
                                } else {
                                    target_pos = *offset;
                                    is_out_of_bounds = false;
                                }
                            }
                            if is_out_of_bounds {
                                let displacement = target_pos - *offset;
                                *velocity += displacement * tension;
                                *velocity *= damping;
                            } else {
                                *velocity *= friction;
                            }
                        };
                    handle_axis(&mut self.offset.x, &mut self.velocity.x, screen_size.x, scaled.x);
                    handle_axis(&mut self.offset.y, &mut self.velocity.y, screen_size.y, scaled.y);
                    if self.velocity.length_sq() <= 0.01 {
                        self.velocity = Vec2::ZERO;
                    }
                }
            }
        }

        self.interacted = false;
    }

    // --- Render --------------------------------------------------------------

    pub fn render(&mut self, renderer: &mut Renderer) -> anyhow::Result<()> {
        // Apply fullscreen toggles requested via key/menu.
        if renderer.is_fullscreen() != self.is_fullscreen {
            renderer.set_fullscreen(self.is_fullscreen);
        }

        renderer.begin_frame();
        let area = Rect::from_min_size(Vec2::ZERO, renderer.drawable_size());

        if let Some(video) = &self.video {
            if let Some((y, uv)) = video.planes() {
                let frame_size =
                    Vec2::new(video.frame_size[0] as f32, video.frame_size[1] as f32);
                let rect = fit_centered(frame_size, area);
                let params = video.video_color().map(|c| VideoColorParams {
                    transfer: match c.transfer {
                        crate::video::Transfer::Sdr => 0,
                        crate::video::Transfer::Pq => 1,
                        crate::video::Transfer::Hlg => 2,
                    },
                    bt2020: c.bt2020_primaries,
                    full_range: c.full_range,
                    peak_nits: c.peak_nits,
                    sdr_white_nits: c.sdr_white_nits,
                }).unwrap_or(VideoColorParams {
                    transfer: 0,
                    bt2020: false,
                    full_range: false,
                    peak_nits: 1000.0,
                    sdr_white_nits: 203.0,
                });
                renderer.draw_video(y, uv, rect, params);
            } else {
                let pos = area.center();
                renderer.draw_text(
                    "Loading video…",
                    18.0,
                    pos,
                    TextAlign::Center,
                    gray(180),
                );
            }
            if let Some(text) = video.current_subtitle() {
                draw_subtitle(renderer, area, &text);
            }
            if let Some(osd) = video.osd_text() {
                draw_osd(renderer, area, &osd);
            }
            // The seek bar / time HUD auto-hides a few seconds after the last
            // interaction and reappears on seek / pause / resume. While scrubbing
            // it stays up and previews the marker at the drag position.
            if video.controls_visible() || self.scrubbing {
                let scrub = self.scrubbing.then_some(self.scrub_frac);
                draw_seek_bar(renderer, area, video.position_secs(), video.duration_secs(), scrub, video.is_looping());
            }
        } else if self.image.is_some() {
            self.render_image(renderer, area);
        } else if let Some(err) = self.last_error.clone() {
            renderer.draw_text(&err, 18.0, area.center(), TextAlign::Center, rgba8(230, 60, 60, 255));
        } else if self.full_res_pending {
            let current_path = self
                .image_files
                .get(self.image_order.get(self.current_index).copied().unwrap_or(usize::MAX));
            let label = match current_path {
                Some(p) => format!("Loading {}…", p.display()),
                None => "Loading…".to_string(),
            };
            renderer.draw_text(&label, 18.0, area.center(), TextAlign::Center, gray(180));
        }

        if self.show_delete_confirmation {
            self.render_delete_dialog(renderer, area);
        }
        if self.context_menu.is_some() {
            self.render_context_menu(renderer, area);
        }

        renderer.end_frame()
    }

    #[allow(clippy::too_many_lines)]
    fn render_image(&mut self, renderer: &mut Renderer, area: Rect) {
        let Some(image) = &mut self.image else { return };
        let full_res_size =
            Vec2::new(image.full_res_image.width() as f32, image.full_res_image.height() as f32);

        let preview_size =
            Vec2::new(image.preview_texture.width() as f32, image.preview_texture.height() as f32);
        let preview_scale = preview_size.x / full_res_size.x;
        let show_tiles = image.needs_tiling && self.zoom > preview_scale;

        if !show_tiles {
            if !image.tile_cache.is_empty() {
                image.tile_cache.clear();
            }
            let scaled_size = full_res_size * self.zoom;
            let image_rect = Rect::from_min_size(area.min + self.offset, scaled_size);
            renderer.draw_texture_full(&image.preview_texture, image_rect, WHITE);
        } else {
            let img_w = image.full_res_image.width();
            let img_h = image.full_res_image.height();
            // Visible region of the image in image-pixel space.
            let vis_min = (Vec2::ZERO - self.offset) / self.zoom;
            let vis_size = area.size() / self.zoom;

            let min_col = (vis_min.x / TILE_SIZE as f32).floor().max(0.0) as usize;
            let max_col = ((vis_min.x + vis_size.x) / TILE_SIZE as f32).ceil().max(0.0) as usize;
            let min_row = (vis_min.y / TILE_SIZE as f32).floor().max(0.0) as usize;
            let max_row = ((vis_min.y + vis_size.y) / TILE_SIZE as f32).ceil().max(0.0) as usize;

            let num_cols = (img_w + TILE_SIZE - 1) / TILE_SIZE;
            let num_rows = (img_h + TILE_SIZE - 1) / TILE_SIZE;

            for row in min_row..max_row.min(num_rows) {
                for col in min_col..max_col.min(num_cols) {
                    let tile_key = (row, col);
                    let (tex, dims) = if let Some((t, d)) = image.tile_cache.get(&tile_key) {
                        (t.clone(), *d)
                    } else {
                        let x_start = col * TILE_SIZE;
                        let y_start = row * TILE_SIZE;
                        let tile_w = (x_start + TILE_SIZE).min(img_w) - x_start;
                        let tile_h = (y_start + TILE_SIZE).min(img_h) - y_start;
                        if tile_w == 0 || tile_h == 0 {
                            continue;
                        }
                        // Copy the tile's RGBA rows out of the full-res buffer.
                        let mut tile_rgba = Vec::with_capacity(tile_w * tile_h * 4);
                        for y in 0..tile_h {
                            let src_y = y_start + y;
                            let row_start = (src_y * img_w + x_start) * 4;
                            tile_rgba.extend_from_slice(
                                &image.full_res_image.rgba[row_start..row_start + tile_w * 4],
                            );
                        }
                        let buf = PixelBuf::new(tile_w as u32, tile_h as u32, tile_rgba);
                        let Ok(tex) = renderer.upload_texture(&buf) else { continue };
                        let dims = [tile_w, tile_h];
                        image.tile_cache.insert(tile_key, (tex.clone(), dims));
                        (tex, dims)
                    };

                    let tile_min_on_screen = area.min
                        + self.offset
                        + Vec2::new((col * TILE_SIZE) as f32, (row * TILE_SIZE) as f32) * self.zoom;
                    let tile_screen_rect = Rect::from_min_size(
                        tile_min_on_screen,
                        Vec2::new(dims[0] as f32, dims[1] as f32) * self.zoom,
                    );
                    if area.intersects(tile_screen_rect) {
                        renderer.draw_texture_full(&tex, tile_screen_rect, WHITE);
                    }
                }
            }
        }

        // Selection border around the (notional) full image rect.
        let scaled_size = full_res_size * self.zoom;
        let image_screen_rect = Rect::from_min_size(area.min + self.offset, scaled_size);
        if area.intersects(image_screen_rect) {
            renderer.stroke_rect(image_screen_rect, 1.0, gray(80));
        }
    }

    fn render_delete_dialog(&self, renderer: &mut Renderer, area: Rect) {
        let path = self.image_files.get(self.image_order[self.current_index]).cloned();
        let msg = match &path {
            Some(p) => format!("Delete '{}'?", p.display()),
            None => "Delete this file?".to_string(),
        };
        // Dim the background.
        renderer.fill_rect(area, rgba8(0, 0, 0, 140));

        let (panel, cancel, delete) = delete_dialog_layout(area);
        renderer.fill_rect(panel, rgba8(40, 40, 40, 245));
        renderer.stroke_rect(panel, 1.0, gray(90));

        renderer.draw_text(
            &msg,
            16.0,
            Vec2::new(panel.center().x, panel.min.y + 24.0),
            TextAlign::Center,
            WHITE,
        );
        self.draw_button(renderer, cancel, "Cancel", false);
        self.draw_button(renderer, delete, "Delete", true);
    }

    /// Draw a labelled button with a hover highlight (`danger` colours destructive
    /// actions red). Hit-testing against the same rect lives in `handle_event`.
    fn draw_button(&self, renderer: &mut Renderer, rect: Rect, label: &str, danger: bool) {
        let hovered = rect.contains(self.mouse_pos);
        let fill = match (danger, hovered) {
            (true, true) => rgba8(190, 70, 70, 255),
            (true, false) => rgba8(150, 50, 50, 255),
            (false, true) => rgba8(95, 95, 95, 255),
            (false, false) => rgba8(70, 70, 70, 255),
        };
        renderer.fill_rect(rect, fill);
        renderer.stroke_rect(rect, 1.0, gray(120));
        renderer.draw_text(
            label,
            15.0,
            Vec2::new(rect.center().x, rect.center().y - 9.0),
            TextAlign::Center,
            WHITE,
        );
    }

    fn render_context_menu(&self, renderer: &mut Renderer, area: Rect) {
        let Some(anchor) = self.context_menu else { return };
        let (panel, rows) = context_menu_layout(anchor, area);
        renderer.fill_rect(panel, rgba8(35, 35, 35, 245));
        renderer.stroke_rect(panel, 1.0, gray(90));

        let items = [
            ("Fullscreen (F)", self.is_fullscreen),
            ("Scale to fit (Enter)", self.is_scaled_to_fit),
            ("Random order", self.is_randomized),
        ];
        for (row, (label, checked)) in rows.iter().zip(items.iter()) {
            if row.contains(self.mouse_pos) {
                renderer.fill_rect(*row, rgba8(255, 255, 255, 28));
            }
            // Checkbox: outlined box, filled when the setting is on.
            let bs = 16.0;
            let bx = Rect::xywh(row.min.x + 8.0, row.center().y - bs / 2.0, bs, bs);
            renderer.stroke_rect(bx, 1.5, gray(200));
            if *checked {
                renderer.fill_rect(
                    Rect::xywh(bx.min.x + 3.0, bx.min.y + 3.0, bs - 6.0, bs - 6.0),
                    rgba8(120, 180, 255, 255),
                );
            }
            renderer.draw_text(
                label,
                15.0,
                Vec2::new(bx.max().x + 10.0, row.center().y - 9.0),
                TextAlign::Left,
                WHITE,
            );
        }
    }

    // --- Main-loop queries ---------------------------------------------------

    /// Whether the app needs continuous frames (vs. blocking until an event).
    pub fn is_active(&self) -> bool {
        if self.should_quit {
            return true;
        }
        if let Some(v) = &self.video {
            // Keep rendering while playing, and while the HUD is fading so it
            // hides/reappears promptly even when paused.
            if v.is_playing() || v.controls_visible() {
                return true;
            }
        }
        if let Some(img) = &self.image {
            if img.animation.as_ref().map(|a| a.frames.len() > 1).unwrap_or(false) {
                return true;
            }
        }
        self.full_res_pending
            || self.dragging
            || self.scrubbing
            || self.velocity.length_sq() > 0.01
    }

    pub fn quit_requested(&self) -> bool {
        self.should_quit
    }

    pub fn shutdown(&mut self) {
        self.shutdown_workers();
        self.video = None;
    }
}
