// --- Image Loading & Helper Functions ---
use image::{codecs::gif::GifDecoder, imageops, AnimationDecoder, DynamicImage, ImageReader, Luma};
use jxl_oxide::integration::JxlDecoder;
use libjpeg_turbo_rs as ljt;
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

use crate::formats::*;
use crate::types::{AnimationFrame, LoadedImage, PixelBuf};

/// Target maximum dimension (in pixels) for the fast preview decode. Matches the
/// `max_texture_side` used when building the preview texture, so the preview we
/// decode is already close to what gets uploaded to the GPU.
pub const PREVIEW_MAX_DIM: u32 = 2048;

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
    } else if matches!(extension.as_str(), "jpg" | "jpeg") {
        load_jpeg_full(&path_str)
    } else {
        load_with_image_crate(&path_str)
    }
}

/// Full-resolution decode for the foreground worker. Returns the displayable
/// image and, for static images, the decoded `DynamicImage` so the caller can
/// generate the preload cache *after* handing the display image to the UI —
/// keeping the (Lanczos resize + JPEG encode + disk write) off the first-paint
/// path. Animated images return `None` (we never cache them).
pub fn load_full_for_worker(path: &Path) -> Result<(LoadedImage, Option<DynamicImage>), String> {
    let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
    if ANIM_SUPPORTED_FORMATS.contains(&extension.as_str()) {
        return Ok((load_animated_gif(&path.to_string_lossy())?, None));
    }
    let dynamic_image = decode_image_data(path)?;
    // Build the display image from a borrow so we keep `dynamic_image` for the
    // cache step. For RGB JPEGs (the common case) this costs nothing extra over
    // `into_rgba8`, which would also allocate a fresh RGBA buffer.
    let color = pixel_buf_from_dynamic(&dynamic_image);
    Ok((LoadedImage::Static(color), Some(dynamic_image)))
}

/// Decode a reduced-resolution preview as cheaply as possible so the user sees a
/// crisp image almost immediately while the full-resolution decode runs behind
/// it. For JPEGs this is a true DCT-scaled decode (roughly an order of magnitude
/// less work than decoding 24MP in full); other formats have no pure-Rust
/// decode-time shortcut, so they decode normally and are downscaled.
pub fn decode_preview(path: &Path, max_dim: u32) -> Result<DynamicImage, String> {
    let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
    if matches!(extension.as_str(), "jpg" | "jpeg") {
        if let Some(img) = load_jpeg_scaled(path, max_dim) {
            return Ok(img);
        }
        // Unsupported subsampling/colour model (e.g. CMYK) — fall through.
    }
    let full = decode_image_data(path)?;
    if full.width() > max_dim || full.height() > max_dim {
        // Triangle (bilinear) is plenty for a transient preview and much faster
        // than the Lanczos3 used for the persisted cache.
        Ok(full.resize(max_dim, max_dim, imageops::FilterType::Triangle))
    } else {
        Ok(full)
    }
}

/// Decode JPEG bytes with libjpeg-turbo-rs (pure-Rust libjpeg-turbo with SIMD).
/// `scale` selects a DCT scaling factor for cheap downscaled decoding; `None`
/// decodes at full resolution. Output is always RGBA8 (the decoder converts
/// grayscale/CMYK/etc. for us). Returns `None` on any failure so the caller can
/// fall back to the format-sniffing `image`-crate path (mislabelled files, etc.).
fn decode_jpeg_turbo(data: &[u8], scale: Option<ljt::ScalingFactor>) -> Option<DynamicImage> {
    let mut decoder = ljt::Decoder::new(data).ok()?;
    if let Some(s) = scale {
        decoder.set_scale(s);
    }
    decoder.set_output_format(ljt::PixelFormat::Rgba);
    let img = decoder.decode_image().ok()?;
    image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.data).map(DynamicImage::ImageRgba8)
}

/// Full-resolution JPEG decode. Tries the fast SIMD libjpeg-turbo path first and
/// falls back to the `image` crate (with content sniffing) for files that aren't
/// actually decodable JPEGs despite a .jpg/.jpeg extension.
fn load_jpeg_full(path: &str) -> Result<DynamicImage, String> {
    let data = fs::read(path).map_err(|e| format!("Failed to open {}: {}", path, e))?;
    if let Some(img) = decode_jpeg_turbo(&data, None) {
        return Ok(img);
    }
    log::debug!("libjpeg-turbo declined {}; falling back to image crate", path);
    load_with_image_crate(path)
}

/// DCT-scaled JPEG decode for fast previews. Reads the header to size the scale
/// factor, then decodes at the smallest libjpeg M/8 factor whose output still
/// covers `max_dim` (crisp preview, a fraction of the full-decode work).
fn load_jpeg_scaled(path: &Path, max_dim: u32) -> Option<DynamicImage> {
    let data = fs::read(path).ok()?;
    let (fw, fh) = jpeg_dimensions_reader(std::io::Cursor::new(&data))?;
    let scale = pick_jpeg_scale(fw.max(fh), max_dim);
    decode_jpeg_turbo(&data, Some(scale))
}

/// Pick a libjpeg `M/8` scaling factor (M in 1..=8): the smallest output that
/// still covers `target` in the largest dimension, or 1/1 if the image is already
/// at or below the target.
fn pick_jpeg_scale(full_max_dim: u32, target: u32) -> ljt::ScalingFactor {
    if full_max_dim <= target {
        return ljt::ScalingFactor::new(8, 8);
    }
    for num in 1..=8u32 {
        if full_max_dim * num / 8 >= target {
            return ljt::ScalingFactor::new(num, 8);
        }
    }
    ljt::ScalingFactor::new(8, 8)
}

/// Read just the JPEG header (markers up to start-of-scan) to get the pixel
/// dimensions without decoding any image data. Used to decide whether a scaled
/// preview decode is actually worthwhile — for an image that's already near the
/// preview size, scaling buys nothing and would just decode the file twice.
pub fn jpeg_dimensions(path: &Path) -> Option<(u32, u32)> {
    let file = fs::File::open(path).ok()?;
    jpeg_dimensions_reader(BufReader::new(file))
}

fn jpeg_dimensions_reader<R: std::io::BufRead>(reader: R) -> Option<(u32, u32)> {
    let mut decoder = jpeg_decoder::Decoder::new(reader);
    decoder.read_info().ok()?;
    let info = decoder.info()?;
    Some((info.width as u32, info.height as u32))
}

/// SIMD/multi-threaded RGBA8 resize via `fast_image_resize` — several times
/// faster than `image::imageops::resize` at equivalent (Lanczos3) quality. Used
/// for both transient preview downscaling and preload-cache generation.
pub fn fast_resize_rgba(src: &[u8], (sw, sh): (u32, u32), (dw, dh): (u32, u32)) -> Vec<u8> {
    use fast_image_resize::images::{Image, ImageRef};
    use fast_image_resize::{PixelType, ResizeOptions, Resizer};

    let src_view = ImageRef::new(sw, sh, src, PixelType::U8x4).expect("rgba source size mismatch");
    let mut dst = Image::new(dw, dh, PixelType::U8x4);
    let mut resizer = Resizer::new();
    // `use_alpha(false)`: resize channels independently, matching the previous
    // `imageops::resize` behaviour. egui's `ColorImage` buffer is already
    // premultiplied, so letting fir multiply/divide alpha again would corrupt
    // semi-transparent pixels.
    resizer
        .resize(&src_view, &mut dst, &ResizeOptions::new().use_alpha(false))
        .expect("resize failed");
    dst.into_vec()
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
        let image = PixelBuf::new(dims.0, dims.1, buffer.into_raw());
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

/// Consume a `DynamicImage` into a tightly-packed RGBA8 [`PixelBuf`] for upload.
pub fn to_pixel_buf(img: DynamicImage) -> PixelBuf {
    let rgba = img.into_rgba8();
    let dims = rgba.dimensions();
    PixelBuf::new(dims.0, dims.1, rgba.into_raw())
}

/// Like `to_pixel_buf` but borrows the source so the caller can keep the
/// `DynamicImage` afterwards (e.g. to generate the preload cache).
pub fn pixel_buf_from_dynamic(img: &DynamicImage) -> PixelBuf {
    let rgba = img.to_rgba8();
    let dims = rgba.dimensions();
    PixelBuf::new(dims.0, dims.1, rgba.into_raw())
}

fn load_with_image_crate(path: &str) -> Result<DynamicImage, String> {
    log::debug!("Loading with image-rs: {}", path);
    // Determine the format from the file's actual magic bytes rather than trusting
    // the extension. Some files carry a .jpg name but are really WebP/PNG/etc.;
    // without this, `decode()` would feed them to the JPEG decoder and fail with
    // e.g. "Illegal start bytes: 5249" (the "RI" of a RIFF/WebP container).
    ImageReader::open(path)
        .map_err(|e| format!("Failed to open {}: {}", path, e))?
        .with_guessed_format()
        .map_err(|e| format!("Failed to read {}: {}", path, e))?
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
        &VIDEO_SUPPORTED_FORMATS[..],
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

/// True if `path` has a recognized video extension. Video files are routed to
/// the ffmpeg-backed player rather than the image decode pipeline.
pub fn is_video_file(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    VIDEO_SUPPORTED_FORMATS.contains(&ext.as_str())
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

pub fn downscale_pixel_buf(image: &PixelBuf, max_size: usize) -> PixelBuf {
    let (width, height) = (image.size[0], image.size[1]);
    let max = max_size as u32;
    if width <= max && height <= max {
        return image.clone();
    }
    let aspect_ratio = width as f32 / height as f32;
    let (new_w, new_h) = if width > height {
        (max, ((max as f32 / aspect_ratio).round() as u32).max(1))
    } else {
        (((max as f32 * aspect_ratio).round() as u32).max(1), max)
    };
    let resized = fast_resize_rgba(&image.rgba, (width, height), (new_w, new_h));
    PixelBuf::new(new_w, new_h, resized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A file whose extension lies about its real format (here: PNG bytes in a
    /// `.jpg`) must still decode by sniffing the magic bytes, rather than failing
    /// with "Illegal start bytes". Regression test for mislabelled images such as
    /// WebP-in-.jpg files exported by some phone cameras.
    #[test]
    fn decodes_when_extension_mismatches_content() {
        let mut img = image::RgbImage::new(8, 6);
        img.put_pixel(0, 0, image::Rgb([10, 20, 30]));
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = env::temp_dir().join(format!("lv_mismatch_{}.jpg", nanos));
        // Write PNG-encoded bytes to a path ending in `.jpg`.
        DynamicImage::ImageRgb8(img)
            .save_with_format(&path, image::ImageFormat::Png)
            .unwrap();

        let decoded = decode_image_data(&path);
        let _ = fs::remove_file(&path);
        let decoded = decoded.expect("mislabelled PNG-in-.jpg should still decode");
        assert_eq!((decoded.width(), decoded.height()), (8, 6));
    }

    /// Encode a JPEG in memory and decode it back through libjpeg-turbo, at full
    /// resolution and at a 1/2 DCT scale, confirming dimensions and that the colour
    /// survives the round-trip.
    #[test]
    fn turbo_decodes_full_and_scaled() {
        let mut src = image::RgbImage::new(800, 600);
        for p in src.pixels_mut() {
            *p = image::Rgb([200, 100, 50]);
        }
        let mut bytes = Vec::new();
        DynamicImage::ImageRgb8(src)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Jpeg)
            .unwrap();

        // Full decode.
        let full = decode_jpeg_turbo(&bytes, None).expect("turbo full decode");
        assert_eq!((full.width(), full.height()), (800, 600));
        let rgba = full.to_rgba8();
        let px = rgba.get_pixel(400, 300).0;
        assert!((px[0] as i32 - 200).abs() < 12 && (px[1] as i32 - 100).abs() < 12, "colour off: {:?}", px);

        // Scaled decode at 1/2.
        let half = decode_jpeg_turbo(&bytes, Some(ljt::ScalingFactor::new(1, 2))).expect("turbo scaled decode");
        assert_eq!((half.width(), half.height()), (400, 300));
    }

    /// Rough decode-speed comparison on a large, hard-to-compress JPEG. Ignored by
    /// default (slow, needs --release for SIMD). Run with:
    ///   cargo test --release bench_jpeg_decode -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench_jpeg_decode() {
        use std::time::Instant;
        // 24MP with a noisy pattern so the decoder actually does work.
        let (w, h) = (6000u32, 4000u32);
        let mut src = image::RgbImage::new(w, h);
        // Smooth, photographic-like content (gradients + low-frequency waves) so the
        // encoded size and decode cost resemble a real photo rather than pure noise.
        for (x, y, p) in src.enumerate_pixels_mut() {
            let fx = x as f32 / w as f32;
            let fy = y as f32 / h as f32;
            let r = (128.0 + 110.0 * (fx * 6.28).sin()) as u8;
            let g = (128.0 + 110.0 * (fy * 9.42).sin()) as u8;
            let b = (255.0 * (fx + fy) * 0.5) as u8;
            *p = image::Rgb([r, g, b]);
        }
        let mut bytes = Vec::new();
        DynamicImage::ImageRgb8(src)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Jpeg)
            .unwrap();
        println!("\nJPEG size: {} KB ({}x{})", bytes.len() / 1024, w, h);

        let runs = 5;
        let bench = |label: &str, f: &dyn Fn()| {
            f(); // warm up
            let t = Instant::now();
            for _ in 0..runs {
                f();
            }
            println!("  {:<28} {:>7.1} ms/decode", label, t.elapsed().as_secs_f64() * 1000.0 / runs as f64);
        };

        bench("zune full -> RGB", &|| {
            image::load_from_memory(&bytes).unwrap();
        });
        bench("zune full -> RGBA", &|| {
            image::load_from_memory(&bytes).unwrap().into_rgba8();
        });
        bench("turbo full -> RGBA", &|| {
            decode_jpeg_turbo(&bytes, None).unwrap();
        });
        bench("turbo 1/4 scaled -> RGBA", &|| {
            decode_jpeg_turbo(&bytes, Some(ljt::ScalingFactor::new(2, 8))).unwrap();
        });
        bench("jpeg-decoder 1/4 scaled", &|| {
            let mut d = jpeg_decoder::Decoder::new(std::io::Cursor::new(&bytes));
            d.scale(1500, 1500).unwrap();
            d.decode().unwrap();
        });
    }

    /// `pick_jpeg_scale` should choose the smallest M/8 output that still covers the
    /// target, and 1/1 when the image is already small enough.
    #[test]
    fn jpeg_scale_selection() {
        // 6000px → smallest M with 6000*M/8 >= 2048 is M=3 (2250).
        let s = pick_jpeg_scale(6000, 2048);
        assert_eq!((s.num, s.denom), (3, 8));
        // Already below target → no scaling.
        let s = pick_jpeg_scale(1600, 2048);
        assert_eq!((s.num, s.denom), (8, 8));
    }
}
