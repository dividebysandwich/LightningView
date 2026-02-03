#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use egui::{epaint::RectShape, Color32, ColorImage, Pos2, Rect, Shape, TextureHandle, Vec2};
use image::{codecs::gif::GifDecoder, imageops, AnimationDecoder, DynamicImage, ImageReader, Luma};
use ndarray::{s, Array, Array2, IxDyn};
use rayon::prelude::*;
use rustronomy_fits as rsf;
use jxl_oxide::integration::JxlDecoder;
use arboard::{Clipboard, ImageData};
use std::{
    collections::HashMap,
    env,
    error::Error,
    fs,
    io::BufReader,
    path::{Path, PathBuf},
    time::{Duration, Instant},
    borrow::Cow,
};

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use crate::windows::*;

// --- Constants ---
const TILE_SIZE: usize = 1024; // Use tiles of 1024x1024 pixels for the detail view

// --- Supported Formats ---
pub const IMAGEREADER_SUPPORTED_FORMATS: [&str; 4] = ["webp", "tif", "tiff", "tga"];
pub const ANIM_SUPPORTED_FORMATS: [&str; 1] = ["gif"];
pub const IMAGE_RS_SUPPORTED_FORMATS: [&str; 9] = ["jpg", "jpeg", "png", "bmp", "svg", "ico", "pnm", "xbm", "xpm"];
pub const RAW_SUPPORTED_FORMATS: [&str; 24] = ["mrw", "arw", "srf", "sr2", "nef", "mef", "orf", "srw", "erf", "kdc", "dcs", "rw2", "raf", "dcr", "dng", "pef", "crw", "iiq", "3fr", "nrw", "mos", "cr2", "ari", "ori"];
pub const FITS_SUPPORTED_FORMATS: [&str; 2] = ["fits", "fit"];
pub const JXL_SUPPORTED_FORMATS: [&str; 1] = ["jxl"];

// --- Advanced Data Structures for Tiled Viewing ---
struct DisplayableImage {
    /// The full-resolution original image, kept in CPU memory.
    full_res_image: ColorImage,
    /// A single, downscaled texture for fast previews when zoomed out.
    preview_texture: TextureHandle,
    /// Cache for detail tiles to avoid re-uploading them to the GPU every frame.
    tile_cache: HashMap<(usize, usize), (TextureHandle, [usize; 2])>,
    /// Does this image actually need tiling, or is it small enough to fit on the GPU?
    needs_tiling: bool,
}

// Simplified enum for loaded image data before GPU upload
enum LoadedImage {
    Static(ColorImage),
    // For simplicity, this advanced example will treat GIFs as static, showing the first frame.
    // A fully tiled animated viewer is significantly more complex.
}

// --- Main Application State ---
struct ImageViewerApp {
    image: Option<DisplayableImage>,
    image_files: Vec<PathBuf>,
    current_index: usize,
    image_order: Vec<usize>,
    zoom: f32,
    offset: Vec2,
    is_scaled_to_fit: bool,
    is_fullscreen: bool,
    is_randomized: bool,
    show_delete_confirmation: bool,
    last_error: Option<String>,
    clipboard: Option<Clipboard>,
}

impl ImageViewerApp {
    fn new(cc: &eframe::CreationContext<'_>, path: Option<PathBuf>, initial_fullscreen: bool) -> Self {
        let mut app = Self {
            image: None,
            image_files: Vec::new(),
            current_index: 0,
            image_order: Vec::new(),
            zoom: 1.0,
            offset: Vec2::ZERO,
            is_scaled_to_fit: true,
            is_fullscreen: initial_fullscreen,
            is_randomized: false,
            show_delete_confirmation: false,
            last_error: None,
            clipboard: Clipboard::new().ok(),
        };
        if let Some(path) = path {
            app.gather_images_from_directory(&path);
            if !app.image_files.is_empty() {
                app.load_image_at_index(app.current_index, &cc.egui_ctx);
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
        let path = &self.image_files[self.image_order[self.current_index]];
        log::info!("Loading image: {}", path.display());
        let start_time = Instant::now();

        match load_image(path) {
            Ok(LoadedImage::Static(full_res_image)) => {
                let max_texture_side = 2048; // TODO: Detect limit
                let needs_tiling = full_res_image.width() > max_texture_side || full_res_image.height() > max_texture_side;

                let preview_image = if needs_tiling {
                    downscale_color_image(full_res_image.clone(), max_texture_side)
                } else {
                    full_res_image.clone()
                };

                let preview_texture = ctx.load_texture(
                    format!("{}_preview", path.display()),
                    preview_image,
                    Default::default(),
                );

                self.image = Some(DisplayableImage {
                    full_res_image,
                    preview_texture,
                    tile_cache: HashMap::new(),
                    needs_tiling,
                });

                self.is_scaled_to_fit = true;
                self.last_error = None;
                log::info!("Loaded '{}' in {:.2?}", path.display(), start_time.elapsed());
            }
            Err(e) => {
                self.last_error = Some(e.clone());
                self.image = None;
                log::error!("Failed to load image: {}", e);
            }
        }
        ctx.request_repaint();
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

        let all_supported_formats: Vec<&str> = [
            &IMAGEREADER_SUPPORTED_FORMATS[..],
            &ANIM_SUPPORTED_FORMATS[..],
            &IMAGE_RS_SUPPORTED_FORMATS[..],
            &RAW_SUPPORTED_FORMATS[..],
            &FITS_SUPPORTED_FORMATS[..],
            &JXL_SUPPORTED_FORMATS[..],
        ]
        .concat();

        if let Ok(entries) = fs::read_dir(parent_dir) {
            let mut files: Vec<PathBuf> = entries
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .filter(|path| {
                    if !path.is_file() {
                        return false;
                    }
                    let path_str = path.to_string_lossy().to_lowercase();
                    all_supported_formats.iter().any(|format| path_str.ends_with(format))
                })
                .collect();

            files.sort_by_key(|name| name.to_string_lossy().to_lowercase());

            if let Some(index) = files.iter().position(|p| p == file_path) {
                self.current_index = index;
            }

            self.image_files = files;
            self.image_order = (0..self.image_files.len()).collect();
        }
    }
    
    fn next_image(&mut self, ctx: &egui::Context) {
        if !self.image_files.is_empty() {
            self.load_image_at_index((self.current_index + 1) % self.image_files.len(), ctx);
        }
    }
    
    fn prev_image(&mut self, ctx: &egui::Context) {
        if !self.image_files.is_empty() {
            self.load_image_at_index((self.current_index + self.image_files.len() - 1) % self.image_files.len(), ctx);
        }
    }
    
    fn first_image(&mut self, ctx: &egui::Context) {
        if !self.image_files.is_empty() {
            self.load_image_at_index(0, ctx);
        }
    }
    
    fn last_image(&mut self, ctx: &egui::Context) {
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
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::F)) {
            self.is_fullscreen = !self.is_fullscreen;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            self.is_scaled_to_fit = !self.is_scaled_to_fit;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Delete)) {
            self.show_delete_confirmation = true;
        }
    }
}

impl eframe::App for ImageViewerApp {
    
fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
    let is_currently_fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
    if self.is_fullscreen != is_currently_fullscreen {
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.is_fullscreen));
    }

    self.handle_keyboard_input(ctx);

    egui::CentralPanel::default()
        .frame(egui::Frame::default().fill(Color32::from_rgb(20, 20, 20)))
        .show(ctx, |ui| {
            if let Some(image) = &mut self.image {
                let available_rect = ui.available_rect_before_wrap();
                let response = ui.allocate_rect(available_rect, egui::Sense::click_and_drag());

                let full_res_size = Vec2::new(image.full_res_image.width() as f32, image.full_res_image.height() as f32);

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
                }

                if response.dragged_by(egui::PointerButton::Primary) {
                    self.offset += response.drag_delta();
                    self.is_scaled_to_fit = false;
                }
                if let Some(hover_pos) = response.hover_pos() {
                    let scroll = ui.input(|i| i.raw_scroll_delta.y);
                    if scroll != 0.0 {
                        let old_zoom = self.zoom;
                        let zoom_delta = (scroll / 200.0) * self.zoom;
                        self.zoom = (self.zoom + zoom_delta).max(0.001);
                        let image_coords = (hover_pos - available_rect.min - self.offset) / old_zoom;
                        self.offset -= image_coords * (self.zoom - old_zoom);
                        self.is_scaled_to_fit = false;
                    }
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
                            let mut rng = rand::thread_rng();
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
            }
        });
        
    if self.show_delete_confirmation {
            let path = self.image_files.get(self.image_order[self.current_index]).cloned();
        egui::Window::new("Delete File")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                if let Some(path) = &path {
                ui.label(format!("Are you sure you want to delete '{}'?", path.display()));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        self.show_delete_confirmation = false;
                    }
                    if ui.button(egui::RichText::new("Delete").color(Color32::RED)).clicked() {
                        if let Err(e) = fs::remove_file(path) {
                            self.last_error = Some(format!("Failed to delete file: {}", e));
                        } else {
                            log::info!("Deleted file: {}", path.display());
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
                                self.load_image_at_index(self.current_index, ctx);
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

// --- Image Loading & Helper Functions ---

fn load_image(path: &Path) -> Result<LoadedImage, String> {
    let path_str = path.to_string_lossy();
    let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
    
    if ANIM_SUPPORTED_FORMATS.contains(&extension.as_str()) {
        let frames = load_animated_gif_frames(&path_str)?;
        let first_frame = frames.into_iter().next().map(|(img, _)| img).ok_or_else(|| "GIF has no frames".to_string())?;
        return Ok(LoadedImage::Static(first_frame));
    }

    log::info!("Loading image: {}", path_str);
    log::info!("Detected format based on extension: {}", extension);

    let dynamic_image = if RAW_SUPPORTED_FORMATS.contains(&extension.as_str()) {
        load_raw(&path_str)
    } else if FITS_SUPPORTED_FORMATS.contains(&extension.as_str()) {
        load_fits(&path_str)
    } else if JXL_SUPPORTED_FORMATS.contains(&extension.as_str()) {
        load_jxl(&path_str)
    } else {
        load_with_image_crate(&path_str)
    }?;

    Ok(LoadedImage::Static(to_egui_color_image(dynamic_image)))
}

fn load_jxl(path: &str) -> Result<DynamicImage, String> {
    log::info!("Loading JXL: {}", path);
    let file = fs::File::open(path).map_err(|e| format!("Failed to open JXL: {}", e))?;
    let reader = BufReader::new(file);
    let decoder = JxlDecoder::new(reader).map_err(|e| format!("Failed to create JXL decoder: {}", e))?;
    let dynamic_image: DynamicImage = DynamicImage::from_decoder(decoder).map_err(|e| format!("Failed to decode JXL: {}", e))?;
    log::info!("Loading image data: {}x{}", dynamic_image.width(), dynamic_image.height());

    Ok(dynamic_image)
}

fn load_animated_gif_frames(path: &str) -> Result<Vec<(ColorImage, Duration)>, String> {
    let file = fs::File::open(path).map_err(|e| format!("Failed to open GIF: {}", e))?;
    let reader = BufReader::new(file);
    let decoder = GifDecoder::new(reader).map_err(|e| format!("Failed to create GIF decoder: {}", e))?;
    let frames = decoder.into_frames().collect_frames().map_err(|e| format!("Failed to decode GIF frames: {}", e))?;

    Ok(frames
        .into_iter()
        .map(|frame| {
            let delay = Duration::from(frame.delay());
            let image_buffer = frame.into_buffer();
            let dims = image_buffer.dimensions();
            let color_image = ColorImage::from_rgba_unmultiplied([dims.0 as _, dims.1 as _], image_buffer.as_raw());
            (color_image, delay)
        })
        .collect())
}

fn to_egui_color_image(img: DynamicImage) -> ColorImage {
    let rgba = img.into_rgba8();
    let dims = rgba.dimensions();
    ColorImage::from_rgba_unmultiplied([dims.0 as _, dims.1 as _], rgba.as_raw())
}

fn load_with_image_crate(path: &str) -> Result<DynamicImage, String> {
    log::debug!("Loading with image-rs: {}", path);
    ImageReader::open(path)
        .map_err(|e| format!("Failed to open {}: {}", path, e))?
        .decode()
        .map_err(|e| format!("Failed to decode {}: {}", path, e))
}

fn load_raw(path: &str) -> Result<DynamicImage, String> {
    log::debug!("Loading RAW: {}", path);
    let mut pipeline = imagepipe::Pipeline::new_from_file(path).map_err(|e| format!("Failed to load RAW: {}", e))?;
    let decoded = pipeline.output_8bit(None).map_err(|e| format!("Failed to process RAW: {}", e))?;

    image::RgbImage::from_raw(decoded.width as u32, decoded.height as u32, decoded.data)
        .map(DynamicImage::ImageRgb8)
        .ok_or_else(|| "Failed to create image from RAW data".to_string())
}

fn load_fits(path: &str) -> Result<DynamicImage, String> {
    log::debug!("Loading FITS: {}", path);
    let mut fits = rsf::Fits::open(Path::new(path)).map_err(|e| format!("FITS open error: {}", e))?;
    let hdu = fits.remove_hdu(0).ok_or_else(|| "FITS HDU error: failed to remove HDU".to_string())?;
    let data = hdu.to_parts().1.ok_or("No data in FITS HDU")?;

    let array = match data {
        rsf::Extension::Image(img) => rgb_to_grayscale(img.as_owned_f32_array()),
        _ => Err("No image data found in FITS".into()),
    }
    .map_err(|e| format!("FITS data conversion error: {}", e))?;

    let (height, width) = (array.shape()[0], array.shape()[1]);
    #[allow(deprecated)]
    let mut data_f32: Vec<f32> = array.into_raw_vec();

    let (min_val, max_val) = data_f32
        .par_iter()
        .fold(|| (f32::MAX, f32::MIN), |(min, max), &x| (min.min(x), max.max(x)))
        .reduce(|| (f32::MAX, f32::MIN), |(a_min, a_max), (b_min, b_max)| (a_min.min(b_min), a_max.max(b_max)));
    let scale = 255.0 / (max_val - min_val).max(1e-5);
    data_f32.par_iter_mut().for_each(|x| *x = (*x - min_val) * scale);

    let log_factor = 3000.0;
    let gamma = 1.5;
    let buffer: Vec<u8> = data_f32
        .par_iter()
        .map(|&x| {
            let log_scaled = 255.0 * (1.0 + log_factor * (x.clamp(0.0, 255.0) / 255.0)).ln() / (1.0 + log_factor).ln();
            ((log_scaled / 255.0).powf(gamma) * 255.0) as u8
        })
        .collect();

    image::ImageBuffer::<Luma<u8>, Vec<u8>>::from_raw(width as u32, height as u32, buffer)
        .map(DynamicImage::ImageLuma8)
        .ok_or_else(|| "Failed to create image from FITS data".to_string())
}

fn rgb_to_grayscale(rgb_image: Result<Array<f32, IxDyn>, Box<dyn Error>>) -> Result<Array2<f32>, Box<dyn Error>> {
    let rgb_array = rgb_image?;
    let shape = rgb_array.shape();
    if shape.len() != 3 || shape[2] != 3 {
        return Err("Invalid shape: Expected (H, W, 3)".into());
    }
    Ok(&rgb_array.slice(s![.., .., 0]) * 0.2989 + &rgb_array.slice(s![.., .., 1]) * 0.5870 + &rgb_array.slice(s![.., .., 2]) * 0.1140)
}

fn get_absolute_path(filename: &str) -> Result<PathBuf, String> {
    let path = Path::new(filename);
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        env::current_dir()
            .map(|mut dir| {
                dir.push(path);
                dir
            })
            .map_err(|e| format!("Failed to get current dir: {}", e))
    }
}

fn downscale_color_image(image: ColorImage, max_size: usize) -> ColorImage {
    let size = image.size;
    let rgba_image = image::RgbaImage::from_raw(size[0] as u32, size[1] as u32, image.pixels.iter().flat_map(|c| c.to_array()).collect()).unwrap();
    let (width, height) = (rgba_image.width(), rgba_image.height());
    let new_dims = if width > max_size as u32 || height > max_size as u32 {
        let aspect_ratio = width as f32 / height as f32;
        if width > height { (max_size as u32, (max_size as f32 / aspect_ratio) as u32) } 
        else { ((max_size as f32 * aspect_ratio) as u32, max_size as u32) }
    } else { (width, height) };
    let resized_img = imageops::resize(&rgba_image, new_dims.0, new_dims.1, imageops::FilterType::Lanczos3);
    ColorImage::from_rgba_unmultiplied([resized_img.width() as _, resized_img.height() as _], resized_img.as_raw())
}

// --- Main Entry Point ---
fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        println!("Usage: {} [/windowed] <imagefile>", args[0]);
        return Ok(());
    }
    
    let mut is_fullscreen = true;
    let mut image_file_arg = &args[1];

    if args[1].eq_ignore_ascii_case("/windowed") {
        if args.len() > 2 {
            is_fullscreen = false;
            image_file_arg = &args[2];
        } else {
            println!("Missing image file after /windowed");
            return Ok(());
        }
    }

    #[cfg(target_os = "windows")]
    {
        if image_file_arg.eq_ignore_ascii_case("/register") {
            return match register_urlhandler() {
                Ok(_) => {
                    println!("Success! Registered as image viewer.");
                    Ok(())
                }
                Err(err) => {
                    println!("Failed to register: {}", err);
                    Ok(())
                }
            };
        } else if image_file_arg.eq_ignore_ascii_case("/unregister") {
            unregister_urlhandler();
            println!("Unregistered as image viewer.");
            return Ok(());
        }
    }

    let initial_path = get_absolute_path(image_file_arg)?;

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0])
            .with_min_inner_size([300.0, 200.0])
	    .with_app_id("lightningview"),
        ..Default::default()
    };

    eframe::run_native(
        "Lightning View (egui)",
        native_options,
        Box::new(|cc| Ok(Box::new(ImageViewerApp::new(cc, Some(initial_path), is_fullscreen)))),
    )?;

    Ok(())
}
