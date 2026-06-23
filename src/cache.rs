// --- Preload Cache ---
use egui::ColorImage;
use image::DynamicImage;
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::decode::fast_resize_rgba;
use crate::types::LoadedImage;

const PRELOAD_CACHE_WIDTH: u32 = 1920;
const PRELOAD_CACHE_TTL_SECS: u64 = 24 * 60 * 60;

fn fnv1a_hash(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub fn preload_cache_dir() -> PathBuf {
    env::temp_dir().join("lightningview")
}

fn preload_cache_filename(path: &Path) -> String {
    let canonical = path.canonicalize();
    let base = canonical.as_deref().unwrap_or(path);
    let mut input = base.to_string_lossy().to_string();
    if let Ok(meta) = path.metadata() {
        input.push('|');
        input.push_str(&meta.len().to_string());
        if let Ok(modified) = meta.modified() {
            if let Ok(dur) = modified.duration_since(UNIX_EPOCH) {
                input.push('|');
                input.push_str(&dur.as_nanos().to_string());
            }
        }
    }
    format!("{:016x}.qoi", fnv1a_hash(input.as_bytes()))
}

pub fn preload_cache_path(path: &Path) -> PathBuf {
    preload_cache_dir().join(preload_cache_filename(path))
}

pub fn save_preload_cache(img: &DynamicImage, image_path: &Path) -> Result<(), String> {
    if img.width() <= PRELOAD_CACHE_WIDTH {
        return Ok(());
    }
    let cache_path = preload_cache_path(image_path);
    if cache_path.exists() {
        return Ok(());
    }
    fs::create_dir_all(preload_cache_dir())
        .map_err(|e| format!("Failed to create preload cache dir: {}", e))?;

    let aspect = img.height() as f32 / img.width() as f32;
    let new_h = ((PRELOAD_CACHE_WIDTH as f32 * aspect).round() as u32).max(1);
    // Fast SIMD resize, then encode as QOI: both the encode here and the decode on
    // revisit are several times faster than the JPEG round-trip this replaced (QOI
    // is also lossless, so cached previews are pixel-exact).
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let resized = fast_resize_rgba(rgba.as_raw(), (w, h), (PRELOAD_CACHE_WIDTH, new_h));
    let encoded = qoi::encode_to_vec(&resized, PRELOAD_CACHE_WIDTH, new_h)
        .map_err(|e| format!("Failed to QOI-encode preload cache: {}", e))?;
    fs::write(&cache_path, encoded)
        .map_err(|e| format!("Failed to write preload cache: {}", e))?;
    log::debug!("Saved preload cache: {}", cache_path.display());
    Ok(())
}

pub fn load_preload_cache(path: &Path) -> Option<LoadedImage> {
    let cache_path = preload_cache_path(path);
    if !cache_path.exists() {
        return None;
    }
    let bytes = match fs::read(&cache_path) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("Failed to read preload cache {}: {}", cache_path.display(), e);
            return None;
        }
    };
    match qoi::decode_to_vec(&bytes) {
        Ok((header, pixels)) => {
            log::debug!("Hit preload cache for {}", path.display());
            let image = ColorImage::from_rgba_unmultiplied(
                [header.width as _, header.height as _],
                &pixels,
            );
            Some(LoadedImage::Static(image))
        }
        Err(e) => {
            log::warn!("Failed to decode preload cache {}: {}", cache_path.display(), e);
            let _ = fs::remove_file(&cache_path);
            None
        }
    }
}

pub fn clear_old_preload_cache() {
    let dir = preload_cache_dir();
    let Ok(entries) = fs::read_dir(&dir) else { return };
    let now = SystemTime::now();
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else { continue };
        let Ok(age) = now.duration_since(modified) else { continue };
        if age.as_secs() > PRELOAD_CACHE_TTL_SECS {
            if fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
    }
    if removed > 0 {
        log::info!("Cleared {} stale preload cache entries", removed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    /// Save a >1920px image to the QOI preload cache and read it back, confirming
    /// the resize + encode + decode round-trip yields a downscaled preview.
    #[test]
    fn preload_cache_qoi_roundtrip() {
        // A real on-disk source file is needed because the cache key hashes the
        // path's metadata (size + mtime).
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let src_path = env::temp_dir().join(format!("lv_cache_src_{}.png", nanos));
        let src = DynamicImage::ImageRgb8(image::RgbImage::new(2400, 1600));
        src.save(&src_path).unwrap();

        save_preload_cache(&src, &src_path).expect("cache save should succeed");
        let cache_path = preload_cache_path(&src_path);
        assert!(cache_path.exists(), "cache file should be written");

        let loaded = load_preload_cache(&src_path);
        let _ = fs::remove_file(&src_path);
        let _ = fs::remove_file(&cache_path);

        match loaded {
            Some(LoadedImage::Static(img)) => {
                assert_eq!(img.width(), PRELOAD_CACHE_WIDTH as usize);
                assert_eq!(img.height(), (PRELOAD_CACHE_WIDTH as f32 * (1600.0 / 2400.0)).round() as usize);
            }
            other => panic!("expected a static cached image, got {:?}", other.is_some()),
        }
    }
}
