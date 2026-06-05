// --- Image Loading & Helper Functions ---
use egui::ColorImage;
use image::{codecs::gif::GifDecoder, imageops, AnimationDecoder, DynamicImage, ImageReader, Luma};
use jxl_oxide::integration::JxlDecoder;
use ndarray::{s, Array, Array2, IxDyn};
use rayon::prelude::*;
use rustronomy_fits as rsf;
use std::{
    env,
    error::Error,
    fs,
    io::BufReader,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::cache::save_preload_cache;
use crate::formats::*;
use crate::types::{AnimationFrame, LoadedImage};

pub fn decode_image_data(path: &Path) -> Result<DynamicImage, String> {
    let path_str = path.to_string_lossy();
    let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();

    if ANIM_SUPPORTED_FORMATS.contains(&extension.as_str()) {
        return load_gif_first_frame(&path_str);
    }

    log::info!("Loading image: {}", path_str);
    log::info!("Detected format based on extension: {}", extension);

    if RAW_SUPPORTED_FORMATS.contains(&extension.as_str()) {
        load_raw(&path_str)
    } else if FITS_SUPPORTED_FORMATS.contains(&extension.as_str()) {
        load_fits(&path_str)
    } else if JXL_SUPPORTED_FORMATS.contains(&extension.as_str()) {
        load_jxl(&path_str)
    } else {
        load_with_image_crate(&path_str)
    }
}

pub fn load_image(path: &Path) -> Result<LoadedImage, String> {
    let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
    if ANIM_SUPPORTED_FORMATS.contains(&extension.as_str()) {
        return load_animated_gif(&path.to_string_lossy());
    }
    let dynamic_image = decode_image_data(path)?;
    let _ = save_preload_cache(&dynamic_image, path);
    Ok(LoadedImage::Static(to_egui_color_image(dynamic_image)))
}

/// Decode every frame of an animated GIF into already-composited RGBA frames with
/// their delays. The `image` crate's GIF `AnimationDecoder` handles frame
/// disposal/compositing, so each returned frame is a full-canvas image. A
/// single-frame GIF is returned as a plain static image so it takes the normal
/// (tile-capable) render path.
fn load_animated_gif(path: &str) -> Result<LoadedImage, String> {
    let file = fs::File::open(path).map_err(|e| format!("Failed to open GIF: {}", e))?;
    let reader = BufReader::new(file);
    let decoder = GifDecoder::new(reader).map_err(|e| format!("Failed to create GIF decoder: {}", e))?;

    let mut frames = Vec::new();
    for (i, frame) in decoder.into_frames().enumerate() {
        let frame = frame.map_err(|e| format!("Failed to decode GIF frame {}: {}", i, e))?;
        // Clamp absurdly short / zero delays (common in GIFs) to a sane minimum so
        // playback doesn't peg the CPU; this matches typical browser behavior.
        let delay = Duration::from(frame.delay()).max(Duration::from_millis(20));
        let buffer = frame.into_buffer();
        let dims = buffer.dimensions();
        let image = ColorImage::from_rgba_unmultiplied([dims.0 as _, dims.1 as _], buffer.as_raw());
        frames.push(AnimationFrame { image, delay });
    }

    match frames.len() {
        0 => Err("GIF has no frames".to_string()),
        1 => Ok(LoadedImage::Static(frames.pop().unwrap().image)),
        _ => Ok(LoadedImage::Animated(frames)),
    }
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

fn load_gif_first_frame(path: &str) -> Result<DynamicImage, String> {
    let file = fs::File::open(path).map_err(|e| format!("Failed to open GIF: {}", e))?;
    let reader = BufReader::new(file);
    let decoder = GifDecoder::new(reader).map_err(|e| format!("Failed to create GIF decoder: {}", e))?;
    let frame = decoder
        .into_frames()
        .next()
        .ok_or_else(|| "GIF has no frames".to_string())?
        .map_err(|e| format!("Failed to decode GIF frame: {}", e))?;
    Ok(DynamicImage::ImageRgba8(frame.into_buffer()))
}

pub fn to_egui_color_image(img: DynamicImage) -> ColorImage {
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

/// Read `dir` and return every supported image file inside it, sorted by
/// lowercased filename so the listing is stable across calls.
pub fn scan_supported_images(dir: &Path) -> Vec<PathBuf> {
    let all_supported_formats: Vec<&str> = [
        &IMAGEREADER_SUPPORTED_FORMATS[..],
        &ANIM_SUPPORTED_FORMATS[..],
        &IMAGE_RS_SUPPORTED_FORMATS[..],
        &RAW_SUPPORTED_FORMATS[..],
        &FITS_SUPPORTED_FORMATS[..],
        &JXL_SUPPORTED_FORMATS[..],
    ]
    .concat();

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
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
    files
}

pub fn get_absolute_path(filename: &str) -> Result<PathBuf, String> {
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

pub fn downscale_color_image(image: ColorImage, max_size: usize) -> ColorImage {
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
