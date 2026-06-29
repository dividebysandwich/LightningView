//! Threaded ffmpeg video decoder. Frames are decoded ahead of time on a worker
//! thread and queued in PTS order; the UI samples them by the audio clock.
//! Ported near-verbatim from sparkplayer (sparkplayer-native `video.rs`).
//!
//! Frames are emitted as native NV12 planes (8-bit 4:2:0); the GPU video shader
//! does the YUV->RGB conversion. The HDR phases extend this to 10-bit P010 input
//! plus PQ/HLG transfer handling.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg::format::Pixel;
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context as Scaler, flag::Flags};
use ffmpeg::util::frame::video::Video;

/// Opto-electronic transfer of the source: standard SDR gamma, or one of the two
/// HDR transfers (PQ / HLG) that need a tone-mapping pass.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transfer {
    Sdr,
    Pq,
    Hlg,
}

/// Bit depth of the decoded planes: NV12 (8-bit) vs P010 (10-bit in 16-bit words).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameDepth {
    Eight,
    Ten,
}

/// Colour description of a video stream, used to pick the GPU colour pipeline.
#[derive(Clone, Copy, Debug)]
pub struct ColorInfo {
    pub transfer: Transfer,
    /// True for BT.2020 primaries (HDR), false for BT.709 (SDR/HD).
    pub bt2020_primaries: bool,
    /// True for full-range (JPEG) luma/chroma, false for limited (MPEG/TV) range.
    pub full_range: bool,
    /// Source peak luminance in nits (mastering display / maxCLL, or a default).
    pub peak_nits: f32,
    /// Target SDR diffuse-white luminance in nits (BT.2408 reference is 203).
    pub sdr_white_nits: f32,
}

impl ColorInfo {
    pub fn is_hdr(&self) -> bool {
        self.transfer != Transfer::Sdr
    }
}

/// A decoded video frame as native YUV 4:2:0 planes — NV12 (8-bit) or P010
/// (10-bit) — both tightly packed (ffmpeg row stride stripped). The GPU video
/// shader does the YUV->RGB (and, for HDR, tone-mapping) conversion.
pub struct VideoFrame {
    pub pts_secs: f64,
    pub width: u32,
    pub height: u32,
    pub chroma_width: u32,
    pub chroma_height: u32,
    pub depth: FrameDepth,
    pub color: ColorInfo,
    /// Luma plane: `width * height * bytes_per_sample`.
    pub y_plane: Vec<u8>,
    /// Interleaved chroma plane: `chroma_width * chroma_height * 2 * bytes_per_sample`.
    pub uv_plane: Vec<u8>,
}

struct SharedQueue {
    frames: VecDeque<VideoFrame>,
    seek_target_ms: Option<i64>,
    eof: bool,
}

pub struct VideoStream {
    inner: Arc<Mutex<SharedQueue>>,
    stop: Arc<AtomicBool>,
    last_drawn_pts_ms: AtomicI64,
    handle: Option<JoinHandle<()>>,
    pub width: u32,
    pub height: u32,
    #[allow(dead_code)]
    pub frame_rate: f64,
    pub duration: Option<Duration>,
    #[allow(dead_code)]
    pub path: PathBuf,
}

const MAX_QUEUED_FRAMES: usize = 12;

impl VideoStream {
    pub fn open(path: &Path) -> Result<Self> {
        ffmpeg::init().ok();
        ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Fatal);
        let ictx = ffmpeg::format::input(&path.to_path_buf())
            .with_context(|| format!("opening video {}", path.display()))?;
        let stream = ictx
            .streams()
            .best(Type::Video)
            .context("no video stream in file")?;
        let stream_index = stream.index();
        let time_base = stream.time_base();
        let avg_frame_rate = stream.avg_frame_rate();
        let frame_rate = if avg_frame_rate.denominator() != 0 {
            avg_frame_rate.numerator() as f64 / avg_frame_rate.denominator() as f64
        } else {
            25.0
        };
        let codec_ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
        let decoder = codec_ctx.decoder().video()?;
        let width = decoder.width();
        let height = decoder.height();
        let format = decoder.format();

        let duration = {
            let dur = stream.duration();
            if dur > 0 {
                Some(Duration::from_secs_f64(
                    dur as f64 * time_base.numerator() as f64 / time_base.denominator() as f64,
                ))
            } else {
                let d = ictx.duration();
                if d > 0 {
                    Some(Duration::from_secs_f64(d as f64 / ffmpeg::ffi::AV_TIME_BASE as f64))
                } else {
                    None
                }
            }
        };

        let inner = Arc::new(Mutex::new(SharedQueue {
            frames: VecDeque::new(),
            seek_target_ms: None,
            eof: false,
        }));
        let stop = Arc::new(AtomicBool::new(false));

        let inner_t = Arc::clone(&inner);
        let stop_t = Arc::clone(&stop);
        let path_t = path.to_path_buf();

        let handle = thread::Builder::new()
            .name("lightningview-video".into())
            .spawn(move || {
                if let Err(e) = decode_loop(
                    path_t,
                    stream_index,
                    width,
                    height,
                    format,
                    time_base,
                    inner_t,
                    stop_t,
                ) {
                    log::warn!("video decode thread exited: {e}");
                }
            })
            .context("spawning video decoder thread")?;

        // The decoder/ictx above existed only to probe metadata; the thread
        // reopens the input for the actual decode.
        drop(decoder);
        drop(ictx);

        Ok(Self {
            inner,
            stop,
            last_drawn_pts_ms: AtomicI64::new(i64::MIN),
            handle: Some(handle),
            width,
            height,
            frame_rate,
            duration,
            path: path.to_path_buf(),
        })
    }

    /// Tell the decoder thread to seek to `target` and discard buffered frames.
    pub fn seek(&self, target: Duration) {
        if let Ok(mut q) = self.inner.lock() {
            q.frames.clear();
            q.seek_target_ms = Some(target.as_millis() as i64);
            q.eof = false;
        }
        self.last_drawn_pts_ms.store(i64::MIN, Ordering::Relaxed);
    }

    /// Whether at least one decoded frame is queued. After a seek the decoder
    /// drops everything before the target, so a queued frame means decoding has
    /// reached the seek point — used to release the post-seek audio hold.
    pub fn has_queued_frame(&self) -> bool {
        self.inner.lock().map(|q| !q.frames.is_empty()).unwrap_or(false)
    }

    /// Whether the whole file has been decoded *and* every queued frame has been
    /// consumed — i.e. the last frame is on screen and there is nothing left to
    /// play. A seek clears `eof`, so this resets after restarting or seeking.
    pub fn is_drained(&self) -> bool {
        self.inner.lock().map(|q| q.eof && q.frames.is_empty()).unwrap_or(false)
    }

    /// PTS (seconds) of every frame currently queued. Test-only: used to assert
    /// that no pre-seek-target frames are buffered.
    #[cfg(test)]
    pub fn queued_pts(&self) -> Vec<f64> {
        self.inner
            .lock()
            .map(|q| q.frames.iter().map(|f| f.pts_secs).collect())
            .unwrap_or_default()
    }

    /// Pop the most-recent frame whose PTS is at or before `target_secs`.
    /// Older frames are discarded. Returns None if no such frame is ready
    /// (decoder still warming up) or if the same frame was already returned.
    pub fn frame_at(&self, target_secs: f64) -> Option<VideoFrame> {
        let target_ms = (target_secs * 1000.0) as i64;
        let mut q = self.inner.lock().ok()?;
        let mut selected: Option<VideoFrame> = None;
        while let Some(front) = q.frames.front() {
            let front_ms = (front.pts_secs * 1000.0) as i64;
            if front_ms <= target_ms {
                selected = q.frames.pop_front();
            } else {
                break;
            }
        }
        let frame = selected?;
        let frame_ms = (frame.pts_secs * 1000.0) as i64;
        let last = self.last_drawn_pts_ms.load(Ordering::Relaxed);
        if frame_ms == last {
            return None;
        }
        self.last_drawn_pts_ms.store(frame_ms, Ordering::Relaxed);
        Some(frame)
    }
}

impl Drop for VideoStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut q) = self.inner.lock() {
            q.frames.clear();
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Default source peak luminance (nits) for HDR content with no MaxCLL /
/// mastering-display metadata — a common HDR10 mastering level.
const DEFAULT_PEAK_NITS: f32 = 1000.0;

/// Map the decoder's colour metadata to our [`ColorInfo`]. `peak_nits` starts at
/// the default and is refined from per-frame MaxCLL / mastering-display side data
/// in the decode loop. BT.2408 puts SDR reference white at 203 nits.
fn detect_color(decoder: &ffmpeg::codec::decoder::Video) -> ColorInfo {
    use ffmpeg::color::TransferCharacteristic as Trc;

    let transfer = match decoder.color_transfer_characteristic() {
        Trc::SMPTE2084 => Transfer::Pq,
        Trc::ARIB_STD_B67 => Transfer::Hlg,
        _ => Transfer::Sdr,
    };
    let bt2020_primaries = matches!(
        decoder.color_primaries(),
        ffmpeg::color::Primaries::BT2020
    );
    let full_range = matches!(decoder.color_range(), ffmpeg::color::Range::JPEG);

    ColorInfo {
        transfer,
        bt2020_primaries,
        full_range,
        peak_nits: DEFAULT_PEAK_NITS,
        sdr_white_nits: 203.0,
    }
}

// FFI layouts of the libavutil HDR side-data structs. bindgen doesn't emit these
// (nothing in the bound headers references them by value), so we mirror their
// stable C ABI and cast the raw `SideData::data()` bytes onto them.
#[repr(C)]
struct AVContentLightMetadata {
    max_cll: std::os::raw::c_uint,
    max_fall: std::os::raw::c_uint,
}

#[repr(C)]
struct AVMasteringDisplayMetadata {
    display_primaries: [[ffmpeg::ffi::AVRational; 2]; 3],
    white_point: [ffmpeg::ffi::AVRational; 2],
    min_luminance: ffmpeg::ffi::AVRational,
    max_luminance: ffmpeg::ffi::AVRational,
    has_primaries: std::os::raw::c_int,
    has_luminance: std::os::raw::c_int,
}

/// Extract the source peak luminance (nits) from a decoded frame's HDR side data.
/// Prefers MaxCLL (the actual peak of the content); falls back to the mastering
/// display's max luminance. Returns `None` when neither is present.
fn read_peak_nits(frame: &Video) -> Option<f32> {
    use ffmpeg::util::frame::side_data::Type;

    // MaxCLL — maximum content light level (CTA-861.3).
    if let Some(sd) = frame.side_data(Type::ContentLightLevel) {
        let data = sd.data();
        if data.len() >= std::mem::size_of::<AVContentLightMetadata>() {
            let cll = unsafe { &*(data.as_ptr() as *const AVContentLightMetadata) };
            if cll.max_cll > 0 {
                return Some(cll.max_cll as f32);
            }
        }
    }

    // Mastering display max luminance (stored as an AVRational in cd/m²).
    if let Some(sd) = frame.side_data(Type::MasteringDisplayMetadata) {
        let data = sd.data();
        if data.len() >= std::mem::size_of::<AVMasteringDisplayMetadata>() {
            let m = unsafe { &*(data.as_ptr() as *const AVMasteringDisplayMetadata) };
            if m.has_luminance != 0 && m.max_luminance.den != 0 {
                let nits = m.max_luminance.num as f32 / m.max_luminance.den as f32;
                if nits > 0.0 {
                    return Some(nits);
                }
            }
        }
    }

    None
}

fn decode_loop(
    path: PathBuf,
    stream_index: usize,
    width: u32,
    height: u32,
    src_format: Pixel,
    time_base: ffmpeg::Rational,
    inner: Arc<Mutex<SharedQueue>>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let mut ictx = ffmpeg::format::input(&path)?;
    let codec_ctx = ffmpeg::codec::context::Context::from_parameters(
        ictx.stream(stream_index).context("stream gone")?.parameters(),
    )?;
    let mut decoder = codec_ctx.decoder().video()?;

    // Read the stream's colour metadata to decide the conversion target. HDR
    // content (PQ / HLG transfer) is normalised to 10-bit P010 so bit depth is
    // preserved into the GPU tone-mapping shader; everything else collapses to
    // 8-bit NV12. Both keep libswscale on its optimised path.
    // `peak_nits` is refined from per-frame MaxCLL / mastering metadata below.
    let mut color = detect_color(&decoder);
    let mut luminance_resolved = false;
    let (target_fmt, depth, bps) = if color.is_hdr() {
        (Pixel::P010LE, FrameDepth::Ten, 2usize)
    } else {
        (Pixel::NV12, FrameDepth::Eight, 1usize)
    };
    log::info!(
        "Video colour: transfer={:?} bt2020={} full_range={} -> {} ({:?})",
        color.transfer,
        color.bt2020_primaries,
        color.full_range,
        if color.is_hdr() { "P010/HDR" } else { "NV12/SDR" },
        depth,
    );

    let mut scaler = Scaler::get(
        src_format,
        width,
        height,
        target_fmt,
        width,
        height,
        Flags::BILINEAR,
    )?;

    let tb_num = time_base.numerator() as f64;
    let tb_den = time_base.denominator() as f64;
    let pts_to_secs = |pts: i64| -> f64 { pts as f64 * tb_num / tb_den };

    // Pull the two planes out of the scaled frame, stripping ffmpeg's row padding
    // so each plane is tightly packed for direct GPU upload. `bps` is bytes per
    // sample (1 for NV12, 2 for P010); the chroma plane interleaves two samples.
    let extract_frame = move |conv: &Video, secs: f64, color: ColorInfo| -> VideoFrame {
        let w = conv.width() as usize;
        let h = conv.height() as usize;
        let cw = w.div_ceil(2);
        let ch = h.div_ceil(2);

        let y_stride = conv.stride(0);
        let y_data = conv.data(0);
        let y_row = w * bps;
        let mut y_plane = Vec::with_capacity(y_row * h);
        for row in 0..h {
            let s = row * y_stride;
            y_plane.extend_from_slice(&y_data[s..s + y_row]);
        }

        let uv_stride = conv.stride(1);
        let uv_data = conv.data(1);
        let uv_row = cw * 2 * bps;
        let mut uv_plane = Vec::with_capacity(uv_row * ch);
        for row in 0..ch {
            let s = row * uv_stride;
            uv_plane.extend_from_slice(&uv_data[s..s + uv_row]);
        }

        VideoFrame {
            pts_secs: secs,
            width: w as u32,
            height: h as u32,
            chroma_width: cw as u32,
            chroma_height: ch as u32,
            depth,
            color,
            y_plane,
            uv_plane,
        }
    };

    // After a seek, frames decoded before this PTS (ms) are dropped rather than
    // queued: ffmpeg seeks to the keyframe preceding the requested time, so the
    // frames between that keyframe and the target must be decoded (they're needed
    // to reconstruct the picture) but should not be shown — otherwise the video
    // fast-forwards through the GOP to catch up to the audio clock.
    let mut skip_until_ms: Option<i64> = None;

    let mut packet = ffmpeg::Packet::empty();
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // Honor a pending seek request from the UI thread.
        let seek_to = inner.lock().ok().and_then(|mut q| q.seek_target_ms.take());
        if let Some(ms) = seek_to {
            let ts = (ms * ffmpeg::ffi::AV_TIME_BASE as i64) / 1000;
            let _ = ictx.seek(ts, ..ts);
            decoder.flush();
            skip_until_ms = Some(ms);
        }

        match packet.read(&mut ictx) {
            Ok(()) => {
                if packet.stream() != stream_index {
                    continue;
                }
                if decoder.send_packet(&packet).is_err() {
                    continue;
                }
                let mut decoded = Video::empty();
                while decoder.receive_frame(&mut decoded).is_ok() {
                    if stop.load(Ordering::Relaxed) {
                        return Ok(());
                    }

                    // If a newer seek arrived while we were skipping toward an
                    // older target, abandon this skip so the outer loop can honor
                    // the latest seek. Without this, rapid successive seeks decode
                    // through every intermediate GOP in turn, and the resync hold
                    // can hit its timeout before reaching the final target — which
                    // then resumes audio ahead of the picture.
                    if skip_until_ms.is_some()
                        && inner.lock().map(|q| q.seek_target_ms.is_some()).unwrap_or(false)
                    {
                        break;
                    }

                    let pts = decoded.pts().unwrap_or(0);
                    let secs = pts_to_secs(pts);

                    // Drop post-seek catch-up frames (keyframe..target) silently.
                    if let Some(thr) = skip_until_ms {
                        if (secs * 1000.0) as i64 >= thr {
                            skip_until_ms = None;
                        } else {
                            continue;
                        }
                    }

                    // Refine the source peak luminance from HDR side data once
                    // (it's constant per stream); attached to the decoded frame.
                    if color.is_hdr() && !luminance_resolved {
                        luminance_resolved = true;
                        if let Some(peak) = read_peak_nits(&decoded) {
                            log::info!("HDR source peak luminance: {peak:.0} nits (from metadata)");
                            color.peak_nits = peak;
                        } else {
                            log::info!(
                                "HDR source peak luminance: {:.0} nits (default; no MaxCLL/mastering metadata)",
                                color.peak_nits
                            );
                        }
                    }

                    let mut conv = Video::empty();
                    if scaler.run(&decoded, &mut conv).is_err() {
                        continue;
                    }
                    let mut pending = Some(extract_frame(&conv, secs, color));

                    // Backpressure: wait until the queue has room.
                    while pending.is_some() {
                        if stop.load(Ordering::Relaxed) {
                            return Ok(());
                        }
                        {
                            let mut q = match inner.lock() {
                                Ok(q) => q,
                                Err(_) => return Ok(()),
                            };
                            if q.seek_target_ms.is_some() {
                                // Stale frame from before the seek — drop it.
                                drop(pending.take());
                                break;
                            }
                            if q.frames.len() < MAX_QUEUED_FRAMES {
                                if let Some(f) = pending.take() {
                                    q.frames.push_back(f);
                                }
                            }
                        }
                        if pending.is_some() {
                            thread::sleep(Duration::from_millis(8));
                        }
                    }
                }
            }
            Err(_) => {
                // EOF or read error — flush decoder and mark eof.
                let _ = decoder.send_eof();
                let mut decoded = Video::empty();
                while decoder.receive_frame(&mut decoded).is_ok() {
                    let pts = decoded.pts().unwrap_or(0);
                    let secs = pts_to_secs(pts);
                    if let Some(thr) = skip_until_ms {
                        if (secs * 1000.0) as i64 >= thr {
                            skip_until_ms = None;
                        } else {
                            continue;
                        }
                    }
                    let mut conv = Video::empty();
                    if scaler.run(&decoded, &mut conv).is_err() {
                        continue;
                    }
                    if let Ok(mut q) = inner.lock() {
                        q.frames.push_back(extract_frame(&conv, secs, color));
                    }
                }
                if let Ok(mut q) = inner.lock() {
                    q.eof = true;
                }
                // Idle wait — keep the thread alive until dropped, occasionally
                // checking for a seek request that would resume decoding.
                while !stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(50));
                    let resume = inner.lock().ok().and_then(|q| q.seek_target_ms);
                    if resume.is_some() {
                        break;
                    }
                }
                if stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                // Reopen the input for the seek.
                ictx = ffmpeg::format::input(&path)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Set `LV_SAMPLE_VIDEO` to a path to exercise real decoding. Ignored by
    /// default so `cargo test` stays hermetic.
    #[test]
    #[ignore]
    fn decodes_at_least_one_frame() {
        let path = std::env::var("LV_SAMPLE_VIDEO").expect("set LV_SAMPLE_VIDEO");
        let stream = VideoStream::open(Path::new(&path)).expect("open video");
        assert!(stream.width > 0 && stream.height > 0);
        // Poll until the decoder warms up and a frame for t=0.5s is ready.
        let mut got = None;
        for _ in 0..200 {
            if let Some(f) = stream.frame_at(0.5) {
                got = Some(f);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let f = got.expect("a frame should be produced");
        let bps = if f.depth == FrameDepth::Ten { 2 } else { 1 };
        assert_eq!(f.y_plane.len(), f.width as usize * f.height as usize * bps);
        assert_eq!(
            f.uv_plane.len(),
            f.chroma_width as usize * f.chroma_height as usize * 2 * bps
        );
    }

    /// After seeking, the decoder must not queue frames from before the seek
    /// target (the keyframe..target catch-up frames), which is what caused the
    /// post-seek fast-forward. Needs a clip at least ~6s long with a GOP longer
    /// than the queue depth before the target. Set `LV_SAMPLE_VIDEO`.
    #[test]
    #[ignore]
    fn seek_drops_pre_target_frames() {
        let path = std::env::var("LV_SAMPLE_VIDEO").expect("set LV_SAMPLE_VIDEO");
        let stream = VideoStream::open(Path::new(&path)).expect("open video");
        let dur = stream.duration.map(|d| d.as_secs_f64()).unwrap_or(10.0);
        let target = (dur * 0.6).clamp(2.0, dur - 1.0);

        // Warm up so the decoder is running.
        for _ in 0..200 {
            if stream.frame_at(0.5).is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        stream.seek(Duration::from_secs_f64(target));

        // No consumer pulls frames, so post-seek the queue fills (via backpressure)
        // with the first frames at/after the target. None should predate it.
        let mut checked = false;
        for _ in 0..400 {
            let pts = stream.queued_pts();
            if !pts.is_empty() {
                let min = pts.iter().cloned().fold(f64::MAX, f64::min);
                assert!(
                    min >= target - 0.2,
                    "queued frame at {min:.3}s precedes seek target {target:.3}s \
                     (would fast-forward to catch up)"
                );
                checked = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(checked, "no frames queued after seek");
    }

    /// Rapid successive seeks must end up decoding toward the *latest* target,
    /// not an intermediate one — otherwise the post-seek resync can stall. Set
    /// `LV_SAMPLE_VIDEO`.
    #[test]
    #[ignore]
    fn rapid_seeks_land_on_latest_target() {
        let path = std::env::var("LV_SAMPLE_VIDEO").expect("set LV_SAMPLE_VIDEO");
        let stream = VideoStream::open(Path::new(&path)).expect("open video");
        let dur = stream.duration.map(|d| d.as_secs_f64()).unwrap_or(20.0);

        for _ in 0..200 {
            if stream.frame_at(0.5).is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        // Seek far (a long skip), then supersede it mid-skip with the real target.
        let far = (dur * 0.8).clamp(2.0, dur - 1.0);
        let final_t = (dur * 0.3).clamp(1.0, dur - 1.0);
        stream.seek(Duration::from_secs_f64(far));
        std::thread::sleep(Duration::from_millis(30));
        stream.seek(Duration::from_secs_f64(final_t));

        let mut checked = false;
        for _ in 0..400 {
            let pts = stream.queued_pts();
            if !pts.is_empty() {
                let min = pts.iter().cloned().fold(f64::MAX, f64::min);
                assert!(
                    min >= final_t - 0.3,
                    "after rapid seeks, queued frame {min:.3}s is not at the latest \
                     target {final_t:.3}s"
                );
                checked = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(checked, "no frames queued after rapid seeks");
    }
}
