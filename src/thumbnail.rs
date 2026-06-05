use image::DynamicImage;
use std::{fs, path::Path};

use crate::formats::RAW_SUPPORTED_FORMATS;

/// Try to extract an embedded preview/thumbnail from the file so the user sees
/// *something* immediately while the (much slower) full-res decode runs in the
/// background. Only called when the mid-res preload cache hasn't been generated
/// yet — once the cache exists for this file, we skip this entirely and use it.
pub fn load_embedded_thumbnail(path: &Path) -> Option<DynamicImage> {
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    if RAW_SUPPORTED_FORMATS.contains(&extension.as_str()) {
        // rawler can panic on malformed files; isolate it so we just fall back
        // to the regular decode path instead of taking the UI thread down.
        return std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let params = rawler::decoders::RawDecodeParams::default();
            rawler::analyze::extract_preview_pixels(path, &params).ok()
        }))
        .ok()
        .flatten();
    }

    let bytes = fs::read(path).ok()?;
    let exif = match extension.as_str() {
        "jpg" | "jpeg" => find_exif_in_jpeg(&bytes)?,
        "png" => find_exif_in_png(&bytes)?,
        _ => return None,
    };
    extract_thumbnail_from_exif(exif)
}

/// Locate the EXIF (APP1) TIFF blob inside a JPEG container.
fn find_exif_in_jpeg(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut i = 2;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            return None;
        }
        let marker = bytes[i + 1];
        // Hit start-of-scan or end-of-image without finding APP1 — give up.
        if marker == 0xDA || marker == 0xD9 {
            return None;
        }
        let seg_len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        if seg_len < 2 || i + 2 + seg_len > bytes.len() {
            return None;
        }
        if marker == 0xE1 && seg_len >= 8 && &bytes[i + 4..i + 10] == b"Exif\0\0" {
            return Some(&bytes[i + 10..i + 2 + seg_len]);
        }
        i += 2 + seg_len;
    }
    None
}

/// Locate the EXIF TIFF blob inside a PNG container's `eXIf` chunk.
fn find_exif_in_png(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() < 8 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let mut i = 8;
    while i + 12 <= bytes.len() {
        let length = u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]) as usize;
        let chunk_type = &bytes[i + 4..i + 8];
        let data_end = i.checked_add(8)?.checked_add(length)?;
        if data_end + 4 > bytes.len() {
            return None;
        }
        if chunk_type == b"eXIf" {
            return Some(&bytes[i + 8..data_end]);
        }
        if chunk_type == b"IEND" {
            return None;
        }
        i = data_end + 4; // skip CRC
    }
    None
}

/// Walk a TIFF blob's IFD1 entries to find the JPEG-compressed thumbnail and
/// decode it. Returns None if the blob isn't a well-formed TIFF, has no IFD1,
/// or the embedded thumbnail isn't a JPEG we can decode.
fn extract_thumbnail_from_exif(tiff: &[u8]) -> Option<DynamicImage> {
    if tiff.len() < 8 {
        return None;
    }
    let little_endian = match &tiff[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let read_u16 = |off: usize| -> Option<u16> {
        let b = tiff.get(off..off + 2)?;
        Some(if little_endian {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    };
    let read_u32 = |off: usize| -> Option<u32> {
        let b = tiff.get(off..off + 4)?;
        Some(if little_endian {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        })
    };

    if read_u16(2)? != 0x002A {
        return None;
    }
    let ifd0_offset = read_u32(4)? as usize;
    let ifd0_count = read_u16(ifd0_offset)? as usize;
    let ifd1_offset_loc = ifd0_offset.checked_add(2)?.checked_add(ifd0_count.checked_mul(12)?)?;
    let ifd1_offset = read_u32(ifd1_offset_loc)? as usize;
    if ifd1_offset == 0 {
        return None;
    }
    let ifd1_count = read_u16(ifd1_offset)? as usize;
    let mut thumb_offset: Option<usize> = None;
    let mut thumb_length: Option<usize> = None;
    for i in 0..ifd1_count {
        let entry = ifd1_offset.checked_add(2)?.checked_add(i.checked_mul(12)?)?;
        let tag = read_u16(entry)?;
        // For LONG-typed values the actual value lives in the 4 bytes at entry+8.
        // ThumbnailOffset/Length are both LONG, so we can read them uniformly.
        let value = read_u32(entry + 8)? as usize;
        if tag == 0x0201 {
            thumb_offset = Some(value);
        } else if tag == 0x0202 {
            thumb_length = Some(value);
        }
    }
    let off = thumb_offset?;
    let len = thumb_length?;
    if len == 0 || off.checked_add(len)? > tiff.len() {
        return None;
    }
    image::load_from_memory_with_format(&tiff[off..off + len], image::ImageFormat::Jpeg).ok()
}
