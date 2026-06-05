// --- Preload Cache ---
use image::{imageops, DynamicImage};
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::decode::to_egui_color_image;
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
    format!("{:016x}.jpg", fnv1a_hash(input.as_bytes()))
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
    let resized = img.resize_exact(PRELOAD_CACHE_WIDTH, new_h, imageops::FilterType::Lanczos3);
    let rgb = resized.to_rgb8();
    DynamicImage::ImageRgb8(rgb)
        .save_with_format(&cache_path, image::ImageFormat::Jpeg)
        .map_err(|e| format!("Failed to save preload cache JPEG: {}", e))?;
    log::debug!("Saved preload cache: {}", cache_path.display());
    Ok(())
}

pub fn load_preload_cache(path: &Path) -> Option<LoadedImage> {
    let cache_path = preload_cache_path(path);
    if !cache_path.exists() {
        return None;
    }
    match image::open(&cache_path) {
        Ok(img) => {
            log::debug!("Hit preload cache for {}", path.display());
            Some(LoadedImage::Static(to_egui_color_image(img)))
        }
        Err(e) => {
            log::warn!("Failed to read preload cache {}: {}", cache_path.display(), e);
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
