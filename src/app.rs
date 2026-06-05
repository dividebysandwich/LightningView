// --- Main Application State ---
use eframe::egui;
use egui::{epaint::RectShape, Color32, ColorImage, Pos2, Rect, Shape, Vec2};
use arboard::{Clipboard, ImageData};
use std::{
    borrow::Cow,
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use crate::cache::{load_preload_cache, preload_cache_path};
use crate::decode::{downscale_color_image, scan_supported_images, to_egui_color_image};
use crate::thumbnail::load_embedded_thumbnail;
use crate::types::{
    Animation, AnimationFrame, DisplayableImage, FullResRequest, FullResWorker, LoadedImage,
    MemoryGate, PreloadState,
};
use crate::workers::{spawn_full_res_worker, spawn_preload_workers};

const TILE_SIZE: usize = 1024; // Use tiles of 1024x1024 pixels for the detail view
/// Maximum time we wait for a full-res decode before assuming the worker is stuck
/// (slow/hung decoder, bad file). After this we respawn the worker and unblock
/// the bulk preload so the app doesn't sit there silently forever.
const FULL_RES_WATCHDOG: Duration = Duration::from_secs(20);

pub struct ImageViewerApp {
    image: Option<DisplayableImage>,
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
}

impl ImageViewerApp {
    pub fn new(cc: &eframe::CreationContext<'_>, path: Option<PathBuf>, initial_fullscreen: bool) -> Self {
        let memory_gate = Arc::new(MemoryGate::new());
        let full_res_worker = Some(spawn_full_res_worker(cc.egui_ctx.clone(), memory_gate.clone()));
        let mut app = Self {
            image: None,
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
        };
        if let Some(path) = path {
            app.gather_images_from_directory(&path);
            if !app.image_files.is_empty() {
                app.load_image_at_index(app.current_index, &cc.egui_ctx);
                app.start_bulk_preload();
            } else {
                app.last_error = Some(format!("No supported images found in directory of '{}'", path.display()));
            }
        } else {
            app.last_error = Some("No image file specified.".to_string());
        }
        app
    }

    fn load_image_at_index(&mut self, index: usize, ctx: &egui::Context) {
        self.current_index = index;
        let path = self.image_files[self.image_order[self.current_index]].clone();
        log::info!("Loading image: {}", path.display());
        let start_time = Instant::now();

        self.is_scaled_to_fit = true;
        self.velocity = Vec2::ZERO;
        self.full_res_pending = false;
        self.full_res_pending_since = None;

        if let Some(LoadedImage::Static(preview)) = load_preload_cache(&path) {
            log::info!("Loaded preload-cache preview for '{}' in {:.2?}", path.display(), start_time.elapsed());
            self.display_loaded_image(preview, &path, ctx);
            self.start_full_res_load(path, ctx);
        } else if let Some(thumb) = load_embedded_thumbnail(&path) {
            log::info!("Loaded embedded thumbnail for '{}' in {:.2?}", path.display(), start_time.elapsed());
            self.display_loaded_image(to_egui_color_image(thumb), &path, ctx);
            self.start_full_res_load(path, ctx);
        } else {
            // No preview available. Route the decode through the worker so the UI
            // thread stays responsive and the user can keep navigating; the central
            // panel will render a "Loading…" placeholder until the reply arrives.
            self.image = None;
            self.last_error = None;
            self.start_full_res_load(path, ctx);
        }
        ctx.request_repaint();
    }

    fn display_loaded_image(&mut self, image: ColorImage, path: &Path, ctx: &egui::Context) {
        let max_texture_side = 2048; // TODO: Detect limit
        let needs_tiling = image.width() > max_texture_side || image.height() > max_texture_side;

        let preview_image = if needs_tiling {
            downscale_color_image(image.clone(), max_texture_side)
        } else {
            image.clone()
        };

        let preview_texture = ctx.load_texture(
            format!("{}_preview", path.display()),
            preview_image,
            Default::default(),
        );

        self.image = Some(DisplayableImage {
            full_res_image: image,
            preview_texture,
            tile_cache: HashMap::new(),
            needs_tiling,
            animation: None,
        });

        self.last_error = None;
    }

    /// Install an animated image for playback. Animated frames always render via
    /// the simple (non-tiled) preview path: each tick the UI swaps `preview_texture`
    /// for the active frame, so we never tile them. The first frame is shown
    /// immediately and `full_res_image` tracks the displayed frame (used for sizing
    /// and clipboard copies).
    fn display_animated_image(&mut self, frames: Vec<AnimationFrame>, path: &Path, ctx: &egui::Context) {
        let first_frame = frames[0].image.clone();
        let preview_texture = ctx.load_texture(
            format!("{}_anim", path.display()),
            first_frame.clone(),
            Default::default(),
        );

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

    fn start_full_res_load(&mut self, path: PathBuf, ctx: &egui::Context) {
        // Starve bulk preload of CPU while the foreground decode is pending — the
        // user's currently-viewed image takes priority over speculative cache fills.
        if let Some(state) = &self.preload_state {
            state.pause.store(true, Ordering::Relaxed);
        }
        let preview_width = self
            .image
            .as_ref()
            .map(|i| i.full_res_image.width() as u32)
            .unwrap_or(0);
        let request = FullResRequest { path: path.clone(), preview_width };
        let send_result = self
            .full_res_worker
            .as_ref()
            .map(|w| w.tx.send(request));
        // If the worker channel is gone (e.g. it panicked out of catch_unwind), respawn it
        // so subsequent navigations still get full-res loads.
        if !matches!(send_result, Some(Ok(()))) {
            log::warn!("Full-res worker unavailable; respawning.");
            let worker = spawn_full_res_worker(ctx.clone(), self.memory_gate.clone());
            let _ = worker.tx.send(FullResRequest { path: path.clone(), preview_width });
            self.full_res_worker = Some(worker);
        }
        self.full_res_pending = true;
        self.full_res_pending_since = Some(Instant::now());
        // Ensure the watchdog gets a chance to run even if the UI stays idle.
        ctx.request_repaint_after(FULL_RES_WATCHDOG);
    }

    fn check_pending_load(&mut self, ctx: &egui::Context) {
        // Watchdog: if a full-res decode hasn't returned for too long, the worker is
        // likely stuck on a slow/bad file. Drop it so the next nav respawns a fresh one.
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
        // Drain all available replies, skipping stale ones (path doesn't match current).
        loop {
            let reply = match worker.rx.try_recv() {
                Ok(r) => r,
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // Worker died; reset so the next nav respawns it.
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

            self.full_res_pending = false;
            self.full_res_pending_since = None;
            if let Some(state) = &self.preload_state {
                state.pause.store(false, Ordering::Relaxed);
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
                        // Preserve the user's current view: the displayed size of the image
                        // (full_res_size * zoom) should stay the same across the swap.
                        self.zoom *= preview_width / new_width;
                    }
                    match loaded {
                        LoadedImage::Static(full_res) => self.display_loaded_image(full_res, &reply.path, ctx),
                        LoadedImage::Animated(frames) => self.display_animated_image(frames, &reply.path, ctx),
                    }
                    log::info!("Swapped in full-res image: {}", reply.path.display());
                    ctx.request_repaint();
                }
                Err(e) => {
                    log::error!("Background full-res load failed for {}: {}", reply.path.display(), e);
                    // If there was no preview to fall back on, surface the error.
                    if self.image.is_none() {
                        self.last_error = Some(e);
                    }
                }
            }
            return;
        }
    }

    fn start_bulk_preload(&mut self) {
        // Shut down any prior worker so we don't have two running.
        if let Some(state) = self.preload_state.take() {
            state.shutdown.store(true, Ordering::Relaxed);
        }
        let n = self.image_files.len();
        if n <= 1 {
            return;
        }
        // Bounce outward from current_index: +1, -1, +2, -2, ... so images closest to the
        // user — in either direction — are preloaded first. A purely forward walk leaves
        // backward navigation hitting un-preloaded images until the queue wraps around.
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
            if paths.len() >= n - 1 { break; }
            let back = (self.current_index + n - d) % n;
            if !seen[back] {
                seen[back] = true;
                paths.push(self.image_files[self.image_order[back]].clone());
            }
            d += 1;
        }
        let state = Arc::new(PreloadState {
            shutdown: std::sync::atomic::AtomicBool::new(false),
            // Mirror current foreground state so a brand-new preload pool doesn't
            // immediately race the in-flight decode it should be yielding to.
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
        // Dropping the worker drops the request Sender, which causes the worker
        // thread's recv() to fail and exit.
        self.full_res_worker = None;
    }

    fn copy_to_clipboard(&mut self) {
        if let (Some(clipboard), Some(image)) = (&mut self.clipboard, &self.image) {
            let image = &image.full_res_image;

            let rgba_bytes: Vec<u8> = image
                .pixels
                .iter()
                .flat_map(|color| color.to_array())
                .collect();

            let image_data = ImageData {
                width: image.width(),
                height: image.height(),
                bytes: Cow::from(rgba_bytes),
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
    /// next time the user navigates. Keeps the currently-viewed image in place,
    /// preserves random-order traversal, and only restarts bulk preload if the
    /// listing actually changed (so this is cheap to call on every navigation).
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
            // Preserve the user's random traversal order: walk the old order, drop
            // entries whose path no longer exists, then append any new files.
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

        // Restore current_index to point at the same path. If that file was deleted
        // externally we clamp so the next nav step lands on a valid entry.
        if let Some(cp) = current_path {
            if let Some(file_idx) = self.image_files.iter().position(|p| p == &cp) {
                if let Some(order_idx) = self.image_order.iter().position(|&i| i == file_idx) {
                    self.current_index = order_idx;
                }
            } else if self.current_index >= self.image_order.len() {
                self.current_index = self.image_order.len().saturating_sub(1);
            }
        }

        // New files may need preloading; restart the bulk pool with the updated set.
        self.start_bulk_preload();
    }

    fn next_image(&mut self, ctx: &egui::Context) {
        self.refresh_directory();
        if !self.image_files.is_empty() {
            self.load_image_at_index((self.current_index + 1) % self.image_files.len(), ctx);
        }
    }

    fn prev_image(&mut self, ctx: &egui::Context) {
        self.refresh_directory();
        if !self.image_files.is_empty() {
            self.load_image_at_index((self.current_index + self.image_files.len() - 1) % self.image_files.len(), ctx);
        }
    }

    fn first_image(&mut self, ctx: &egui::Context) {
        self.refresh_directory();
        if !self.image_files.is_empty() {
            self.load_image_at_index(0, ctx);
        }
    }

    fn last_image(&mut self, ctx: &egui::Context) {
        self.refresh_directory();
        if !self.image_files.is_empty() {
            self.load_image_at_index(self.image_files.len() - 1, ctx);
        }
    }

    fn handle_keyboard_input(&mut self, ctx: &egui::Context) {

        let events = ctx.input(|i| i.events.clone());
        // Iterate over all events that occurred this frame.
        for event in &events {
            // Pattern match to find the `Copy` event.
            if let egui::Event::Copy = event {
                log::info!("Copying image to clipboard...");
                self.copy_to_clipboard();
            }
        }

        if ctx.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
            self.next_image(ctx);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
            self.prev_image(ctx);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Home)) {
            self.first_image(ctx);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::End)) {
            self.last_image(ctx);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.show_delete_confirmation {
                self.show_delete_confirmation = false;
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::F)) {
            self.is_fullscreen = !self.is_fullscreen;
        }
        if !self.show_delete_confirmation && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            self.is_scaled_to_fit = !self.is_scaled_to_fit;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Delete)) {
            self.show_delete_confirmation = true;
        }
    }
}

impl eframe::App for ImageViewerApp {

fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
    let ctx = ui.ctx().clone();
    let is_currently_fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
    if self.is_fullscreen != is_currently_fullscreen {
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.is_fullscreen));
    }

    self.handle_keyboard_input(&ctx);
    self.check_pending_load(&ctx);

    egui::CentralPanel::default()
        .frame(egui::Frame::default().fill(Color32::from_rgb(20, 20, 20)))
        .show_inside(ui, |ui| {
            if let Some(image) = &mut self.image {
                // Advance animation playback (GIFs). Frames are pre-decoded; we step
                // forward by wall-clock time and re-upload the active frame whenever it
                // changes, then schedule a repaint for when the next frame is due.
                if let Some(anim) = &mut image.animation {
                    if anim.frames.len() > 1 {
                        let mut changed = false;
                        // Advance past every frame whose display time has elapsed. The
                        // step cap guards against a stalled UI (or pathological tiny
                        // delays) making us spin through the whole animation.
                        let mut steps = 0;
                        while steps < anim.frames.len()
                            && anim.frame_started.elapsed() >= anim.frames[anim.current].delay
                        {
                            anim.frame_started += anim.frames[anim.current].delay;
                            anim.current = (anim.current + 1) % anim.frames.len();
                            changed = true;
                            steps += 1;
                        }
                        // If we were so far behind we hit the cap, resync to "now" so we
                        // don't keep racing to catch up frame-by-frame.
                        if steps == anim.frames.len() {
                            anim.frame_started = Instant::now();
                        }
                        if changed {
                            let frame_image = anim.frames[anim.current].image.clone();
                            image.preview_texture = ctx.load_texture(
                                "anim_frame",
                                frame_image.clone(),
                                Default::default(),
                            );
                            // Keep full_res_image pointed at the displayed frame so
                            // clipboard copies grab what the user actually sees.
                            image.full_res_image = frame_image;
                        }
                        let remaining = anim.frames[anim.current]
                            .delay
                            .saturating_sub(anim.frame_started.elapsed());
                        ctx.request_repaint_after(remaining);
                    }
                }

                let available_rect = ui.available_rect_before_wrap();
                let response = ui.allocate_rect(available_rect, egui::Sense::click_and_drag());

                let full_res_size = Vec2::new(image.full_res_image.width() as f32, image.full_res_image.height() as f32);

                // Handle Scale to Fit
                if self.is_scaled_to_fit {
                    let aspect_ratio = full_res_size.x / full_res_size.y;
                    let available_aspect = available_rect.width() / available_rect.height();
                    let mut fit_size = available_rect.size();
                    if aspect_ratio > available_aspect {
                        fit_size.y = fit_size.x / aspect_ratio;
                    } else {
                        fit_size.x = fit_size.y * aspect_ratio;
                    }
                    self.zoom = fit_size.x / full_res_size.x;
                    self.offset = (available_rect.size() - fit_size) / 2.0;

                    // Kill velocity when in fit mode
                    self.velocity = Vec2::ZERO;
                }

                let mut is_interacting = false;

                // Handle Dragging & Inertia
                if response.dragged_by(egui::PointerButton::Primary) {
                    let delta = response.drag_delta();
                    self.offset += delta;
                    self.velocity = delta; // Capture momentum
                    self.is_scaled_to_fit = false;
                    is_interacting = true;
                } else {
                    // Apply velocity to position first (let it slide)
                    self.offset += self.velocity;
                }

                // Handle Zooming
                if let Some(hover_pos) = response.hover_pos() {
                    let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                    if scroll != 0.0 {
                        let old_zoom = self.zoom;
                        let zoom_delta = (scroll / 200.0) * self.zoom;
                        self.zoom = (self.zoom + zoom_delta).max(0.001);
                        let image_coords = (hover_pos - available_rect.min - self.offset) / old_zoom;
                        self.offset -= image_coords * (self.zoom - old_zoom);
                        self.is_scaled_to_fit = false;
                        self.velocity = Vec2::ZERO;
                        is_interacting = true;
                    }
                }

                // Bouncing & Constraints
                if !self.is_scaled_to_fit && !is_interacting {
                    let screen_size = available_rect.size();
                    let scaled_image_size = full_res_size * self.zoom;

                    let friction = 0.92;    // Slipperyness on valid surface (0.0 - 1.0)
                    let tension = 0.06;     // Spring stiffness (strength of snap back)
                    let damping = 0.65;     // Spring damping (prevents endless bouncing)

                    // Helper closure for axis physics
                    let handle_axis = |offset: &mut f32, velocity: &mut f32, view_dim: f32, img_dim: f32| {
                        let target_pos;
                        let is_out_of_bounds;

                        if img_dim <= view_dim {
                            // If image is smaller than screen, target is the center
                            target_pos = (view_dim - img_dim) / 2.0;
                            // Consider it "out of bounds" if it's not centered
                            is_out_of_bounds = (*offset - target_pos).abs() > 0.5;
                        } else {
                            // If image is larger, check edges
                            let min = view_dim - img_dim; // Far right/bottom edge
                            let max = 0.0;                // Far left/top edge

                            if *offset > max {
                                target_pos = max;
                                is_out_of_bounds = true;
                            } else if *offset < min {
                                target_pos = min;
                                is_out_of_bounds = true;
                            } else {
                                target_pos = *offset; // No target, effectively
                                is_out_of_bounds = false;
                            }
                        }

                        if is_out_of_bounds {
                            // Apply spring force toward target
                            let displacement = target_pos - *offset;
                            *velocity += displacement * tension; // Accelerate towards edge
                            *velocity *= damping;                // Slow down (dampen oscillation)
                        } else {
                            // Standard friction when inside bounds
                            *velocity *= friction;
                        }
                    };

                    handle_axis(&mut self.offset.x, &mut self.velocity.x, screen_size.x, scaled_image_size.x);
                    handle_axis(&mut self.offset.y, &mut self.velocity.y, screen_size.y, scaled_image_size.y);

                    // Stop simulation if movement is negligible to save CPU
                    if self.velocity.length_sq() > 0.01 {
                        ctx.request_repaint();
                    } else {
                        self.velocity = Vec2::ZERO;
                    }
                }

                // We keep repainting as long as the image is moving significantly
                if self.velocity.length_sq() > 0.1 {
                    ctx.request_repaint();
                } else {
                    self.velocity = Vec2::ZERO;
                }

                let preview_size = image.preview_texture.size_vec2();
                let preview_scale = preview_size.x / full_res_size.x;
                let show_tiles = image.needs_tiling && self.zoom > preview_scale;

                if !show_tiles {
                    if !image.tile_cache.is_empty() {
                        log::debug!("Zoomed out, clearing tile cache of {} textures.", image.tile_cache.len());
                        image.tile_cache.clear();
                    }

                    let scaled_size = full_res_size * self.zoom;
                    let image_rect = Rect::from_min_size(available_rect.min + self.offset, scaled_size);
                    ui.painter().image(
                        image.preview_texture.id(),
                        image_rect,
                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                        Color32::WHITE,
                    );
                } else {
                    let screen_offset_in_image_pixels = (available_rect.min - (available_rect.min + self.offset)) / self.zoom;
                    let screen_size_in_image_pixels = available_rect.size() / self.zoom;
                    let visible_image_rect = Rect::from_min_size(
                        Pos2::new(screen_offset_in_image_pixels.x, screen_offset_in_image_pixels.y),
                        screen_size_in_image_pixels,
                    );

                    let min_col_f = visible_image_rect.min.x / TILE_SIZE as f32;
                    let max_col_f = visible_image_rect.max.x / TILE_SIZE as f32;
                    let min_row_f = visible_image_rect.min.y / TILE_SIZE as f32;
                    let max_row_f = visible_image_rect.max.y / TILE_SIZE as f32;

                    // Clamp the tile loop bounds to the actual tile grid of the image to prevent visual glitches.
                    let num_cols = (image.full_res_image.width() + TILE_SIZE - 1) / TILE_SIZE;
                    let num_rows = (image.full_res_image.height() + TILE_SIZE - 1) / TILE_SIZE;

                    let row_start = (min_row_f.floor() as i32).max(0) as usize;
                    let row_end = (max_row_f.ceil() as i32).max(0) as usize;
                    let col_start = (min_col_f.floor() as i32).max(0) as usize;
                    let col_end = (max_col_f.ceil() as i32).max(0) as usize;

                    for row in row_start..row_end.min(num_rows) {
                        for col in col_start..col_end.min(num_cols) {
                            let tile_key = (row, col);

                            // Get both texture and dimensions from cache, or create and cache both.
                            let (texture_id, tile_dims) = if let Some((texture, dims)) = image.tile_cache.get(&tile_key) {
                                (texture.id(), *dims)
                            } else {
                                let x_start = col * TILE_SIZE;
                                let y_start = row * TILE_SIZE;
                                // Calculate the actual width and height of this tile, clamping to image edges
                                let tile_w = (x_start + TILE_SIZE).min(image.full_res_image.width()) - x_start;
                                let tile_h = (y_start + TILE_SIZE).min(image.full_res_image.height()) - y_start;

                                if tile_w == 0 || tile_h == 0 { continue; }

                                // Manually copy the pixel data row by row
                                let mut tile_pixels = Vec::with_capacity(tile_w * tile_h);
                                for y in 0..tile_h {
                                    let src_y = y_start + y;
                                    let row_start_index = src_y * image.full_res_image.width();
                                    let row_slice_start = row_start_index + x_start;
                                    tile_pixels.extend_from_slice(&image.full_res_image.pixels[row_slice_start..row_slice_start + tile_w]);
                                }

                                let tile_image = ColorImage { size: [tile_w, tile_h], pixels: tile_pixels, source_size: Vec2::new(tile_w as f32, tile_h as f32) };

                                let texture = ctx.load_texture(format!("tile_{}_{}", row, col), tile_image, Default::default());
                                let id = texture.id();
                                let dims = [tile_w, tile_h];
                                image.tile_cache.insert(tile_key, (texture, dims));
                                (id, dims)
                            };

                            let tile_min_in_image_pixels = Pos2::new((col * TILE_SIZE) as f32, (row * TILE_SIZE) as f32);
                            let tile_min_on_screen = available_rect.min + self.offset + tile_min_in_image_pixels.to_vec2() * self.zoom;

                            // Use the actual tile dimensions for drawing, not the fixed TILE_SIZE.
                            let tile_dims_vec = Vec2::new(tile_dims[0] as f32, tile_dims[1] as f32);
                            let tile_screen_rect = Rect::from_min_size(tile_min_on_screen, tile_dims_vec * self.zoom);

                            if available_rect.intersects(tile_screen_rect) {
                                ui.painter().image(texture_id, tile_screen_rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                            }
                        }
                    }
                }

                let scaled_size = full_res_size * self.zoom;
                let image_screen_rect = Rect::from_min_size(available_rect.min + self.offset, scaled_size);
                if ui.clip_rect().intersects(image_screen_rect) {
                    ui.painter().add(Shape::Rect(RectShape::stroke(image_screen_rect, 0.0, (1.0, Color32::from_gray(80)), egui::StrokeKind::Outside)));
                }

                response.context_menu(|ui| {
                    if ui.checkbox(&mut self.is_fullscreen, "Fullscreen (F)").clicked() {
                        ui.close();
                    };
                    if ui.checkbox(&mut self.is_scaled_to_fit, "Scale to fit (Enter)").clicked() {
                        ui.close();
                    };
                    if ui.checkbox(&mut self.is_randomized, "Random order").clicked() {
                        if self.is_randomized {
                            let current_image_index = self.image_order[self.current_index];
                            #[allow(deprecated)]
                            let mut rng = rand::rng();
                            use rand::seq::SliceRandom;
                            self.image_order.shuffle(&mut rng);
                            if let Some(pos) = self.image_order.iter().position(|&i| i == current_image_index) {
                                self.current_index = pos;
                            }
                        } else {
                            let current_image_index = self.image_order[self.current_index];
                            self.image_order = (0..self.image_files.len()).collect();
                            if let Some(pos) = self.image_order.iter().position(|&i| i == current_image_index) {
                                self.current_index = pos;
                            }
                        }
                        ui.close();
                    };
                });

            } else if let Some(err) = &self.last_error {
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new(err).color(Color32::RED).size(18.0));
                });
            } else if self.full_res_pending {
                let current_path = self
                    .image_files
                    .get(self.image_order.get(self.current_index).copied().unwrap_or(usize::MAX));
                let label = match current_path {
                    Some(p) => format!("Loading {}…", p.display()),
                    None => "Loading…".to_string(),
                };
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new(label).color(Color32::from_gray(180)).size(18.0));
                });
            }
        });

    if self.show_delete_confirmation {
            let path = self.image_files.get(self.image_order[self.current_index]).cloned();
        egui::Window::new("Delete File")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(&ctx, |ui| {
                if let Some(path) = &path {
                ui.label(format!("Are you sure you want to delete '{}'?", path.display()));
                ui.add_space(10.0);
                let confirm_with_enter = ctx.input(|i| i.key_pressed(egui::Key::Enter));
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        self.show_delete_confirmation = false;
                    }
                    if ui.button(egui::RichText::new("Delete").color(Color32::RED)).clicked() || confirm_with_enter {
                        // Compute the cache path *before* deleting the source — the cache
                        // filename hash incorporates the file's size and mtime, which
                        // become unavailable once the file is gone.
                        let cache_path = preload_cache_path(path);
                        if let Err(e) = fs::remove_file(path) {
                            self.last_error = Some(format!("Failed to delete file: {}", e));
                        } else {
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
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            } else {
                                self.current_index %= self.image_files.len();
                                self.load_image_at_index(self.current_index, &ctx);
                            }
                        }
                        self.show_delete_confirmation = false;
                    }
                });
                }
            });
        }
    }
}

impl Drop for ImageViewerApp {
    fn drop(&mut self) {
        self.shutdown_workers();
    }
}
