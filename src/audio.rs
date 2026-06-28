//! Audio playback for video files: an ffmpeg-backed `rodio::Source` that decodes
//! and resamples to stereo f32, a sample tap that doubles as the master clock
//! for A/V sync, and a thin `AudioPlayer` over a rodio sink supporting multiple
//! audio tracks and seeking. Ported from sparkplayer (sparkplayer-native
//! `audio.rs` + sparkplayer-core `audio_tap.rs`), trimmed to the video path —
//! LightningView only ever feeds it video containers.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg::format::sample::{Sample, Type as SampleType};
use ffmpeg::media::Type as MediaType;
use ffmpeg::software::resampling::Context as Resampler;
use ffmpeg::util::frame::audio::Audio;
use ffmpeg::ChannelLayout;
use rodio::source::Source;
use rodio::{ChannelCount, DeviceSinkBuilder, MixerDeviceSink, Player, SampleRate};

use crate::subtitles::language_display_name;

const TAP_CAPACITY: usize = 8192;

/// The audio sample tap. The playback thread pushes every decoded sample
/// through it; `position()` derives the current playback time from the running
/// sample count, which the UI uses as the A/V master clock.
#[derive(Clone, Default)]
pub struct SampleBuffer {
    inner: Arc<Mutex<SampleBufferInner>>,
}

#[derive(Default)]
struct SampleBufferInner {
    write: usize,
    samples_consumed: u64,
    channels: u16,
    sample_rate: u32,
    base_offset_secs: f64,
}

impl SampleBuffer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(SampleBufferInner {
                write: 0,
                samples_consumed: 0,
                channels: 2,
                sample_rate: 44100,
                base_offset_secs: 0.0,
            })),
        }
    }

    /// Push a single interleaved sample, on the playback thread.
    pub fn push(&self, _sample: f32) {
        if let Ok(mut g) = self.inner.lock() {
            g.write = (g.write + 1) % TAP_CAPACITY;
            g.samples_consumed += 1;
        }
    }

    pub fn set_format(&self, channels: u16, sample_rate: u32) {
        if let Ok(mut g) = self.inner.lock() {
            g.channels = channels.max(1);
            g.sample_rate = sample_rate.max(1);
        }
    }

    pub fn reset(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.write = 0;
            g.samples_consumed = 0;
            g.base_offset_secs = 0.0;
        }
    }

    pub fn set_base_offset(&self, offset: Duration) {
        if let Ok(mut g) = self.inner.lock() {
            g.base_offset_secs = offset.as_secs_f64();
        }
    }

    pub fn position(&self) -> Duration {
        let g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Duration::ZERO,
        };
        let frames = g.samples_consumed / g.channels.max(1) as u64;
        let secs = g.base_offset_secs + frames as f64 / g.sample_rate.max(1) as f64;
        Duration::from_secs_f64(secs.max(0.0))
    }
}

/// Audio source backed by an ffmpeg input. Pulls and demuxes lazily so the
/// decode work happens on rodio's playback thread.
pub struct FfmpegAudioSource {
    ictx: ffmpeg::format::context::Input,
    decoder: ffmpeg::codec::decoder::Audio,
    resampler: Resampler,
    stream_index: usize,
    stream_time_base: ffmpeg::Rational,
    out_channels: u16,
    out_rate: u32,
    duration: Option<Duration>,
    buffer: VecDeque<f32>,
    finished: bool,
    /// Set on seek. The next decoded frame at-or-after this PTS becomes the
    /// first sample we emit; earlier ones (from keyframe-aligned demux seek)
    /// are dropped so the tap's base_offset corresponds to the actual audio.
    pending_seek_secs: Option<f64>,
}

enum FrameDisposition {
    DropAll,
    Keep { skip_interleaved: usize },
}

impl FfmpegAudioSource {
    /// Open the best audio stream of `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let ictx = open_input(path)?;
        let stream_index = ictx
            .streams()
            .best(MediaType::Audio)
            .context("file has no audio stream")?
            .index();
        Self::from_input(ictx, stream_index)
    }

    /// Open a specific audio stream of `path` (used to switch between the
    /// multiple audio tracks a video container may carry).
    pub fn open_stream(path: &Path, stream_index: usize) -> Result<Self> {
        let ictx = open_input(path)?;
        Self::from_input(ictx, stream_index)
    }

    fn from_input(ictx: ffmpeg::format::context::Input, stream_index: usize) -> Result<Self> {
        let stream = ictx
            .stream(stream_index)
            .context("audio stream index out of range")?;
        let time_base = stream.time_base();
        let duration = {
            let dur = stream.duration();
            if dur > 0 {
                Some(Duration::from_secs_f64(
                    dur as f64 * time_base.numerator() as f64 / time_base.denominator() as f64,
                ))
            } else {
                let d = ictx.duration();
                if d > 0 {
                    Some(Duration::from_secs_f64(
                        d as f64 / ffmpeg::ffi::AV_TIME_BASE as f64,
                    ))
                } else {
                    None
                }
            }
        };

        let codec_ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
        let decoder = codec_ctx.decoder().audio()?;

        let in_rate = decoder.rate();
        let in_channels = decoder.channels();
        let in_layout = if decoder.channel_layout() == ChannelLayout::default(0) {
            ChannelLayout::default(in_channels as i32)
        } else {
            decoder.channel_layout()
        };
        let in_format = decoder.format();

        let out_rate: u32 = if in_rate == 0 { 44_100 } else { in_rate };
        let out_layout = ChannelLayout::STEREO;
        let out_channels: u16 = 2;

        let resampler = Resampler::get(
            in_format,
            in_layout,
            in_rate.max(1),
            Sample::F32(SampleType::Packed),
            out_layout,
            out_rate,
        )
        .context("creating audio resampler")?;

        Ok(Self {
            ictx,
            decoder,
            resampler,
            stream_index,
            stream_time_base: time_base,
            out_channels,
            out_rate,
            duration,
            buffer: VecDeque::with_capacity(8192),
            finished: false,
            pending_seek_secs: None,
        })
    }

    /// Seek the underlying input to `target` and reset decoder state.
    pub fn seek(&mut self, target: Duration) -> Result<()> {
        let ts = (target.as_micros() as i64) * (ffmpeg::ffi::AV_TIME_BASE as i64) / 1_000_000;
        self.ictx.seek(ts, ..ts).ok();
        self.decoder.flush();
        self.buffer.clear();
        self.finished = false;
        self.pending_seek_secs = Some(target.as_secs_f64());
        Ok(())
    }

    fn frame_disposition(&mut self, frame: &Audio) -> FrameDisposition {
        let Some(target) = self.pending_seek_secs else {
            return FrameDisposition::Keep { skip_interleaved: 0 };
        };
        let Some(pts) = frame.pts() else {
            self.pending_seek_secs = None;
            return FrameDisposition::Keep { skip_interleaved: 0 };
        };
        let tb_num = self.stream_time_base.numerator() as f64;
        let tb_den = self.stream_time_base.denominator() as f64;
        if tb_den == 0.0 {
            self.pending_seek_secs = None;
            return FrameDisposition::Keep { skip_interleaved: 0 };
        }
        let frame_pts_secs = pts as f64 * tb_num / tb_den;
        let in_rate = frame.rate() as f64;
        let frame_dur_secs = if in_rate > 0.0 {
            frame.samples() as f64 / in_rate
        } else {
            0.0
        };
        if frame_pts_secs + frame_dur_secs <= target {
            return FrameDisposition::DropAll;
        }
        if frame_pts_secs >= target {
            self.pending_seek_secs = None;
            return FrameDisposition::Keep { skip_interleaved: 0 };
        }
        let skip_per_channel = ((target - frame_pts_secs) * self.out_rate as f64).round() as i64;
        let skip_per_channel = skip_per_channel.max(0) as usize;
        let skip_interleaved = skip_per_channel.saturating_mul(self.out_channels as usize);
        self.pending_seek_secs = None;
        FrameDisposition::Keep { skip_interleaved }
    }

    fn ingest_frame(&mut self, decoded: &Audio) {
        let skip = match self.frame_disposition(decoded) {
            FrameDisposition::DropAll => return,
            FrameDisposition::Keep { skip_interleaved } => skip_interleaved,
        };
        let mut resampled = Audio::empty();
        if self.resampler.run(decoded, &mut resampled).is_err() {
            return;
        }
        let before = self.buffer.len();
        self.append_samples(&resampled);
        if skip > 0 {
            let added = self.buffer.len() - before;
            let to_drain = skip.min(added);
            self.buffer.drain(before..before + to_drain);
        }
    }

    fn drain_decoder(&mut self) {
        let mut decoded = Audio::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            self.ingest_frame(&decoded);
        }
    }

    fn append_samples(&mut self, frame: &Audio) {
        let samples = frame.samples();
        if samples == 0 {
            return;
        }
        let bytes = frame.data(0);
        let needed_bytes = samples
            .saturating_mul(self.out_channels as usize)
            .saturating_mul(std::mem::size_of::<f32>());
        let usable = bytes.len().min(needed_bytes);
        if usable < std::mem::size_of::<f32>() {
            return;
        }
        // SAFETY: ffmpeg audio buffers are 4-byte aligned for f32 and `usable`
        // is a multiple of sizeof(f32) by construction.
        let n_f32 = usable / std::mem::size_of::<f32>();
        let interleaved: &[f32] =
            unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const f32, n_f32) };
        self.buffer.extend(interleaved.iter().copied());
    }

    fn fill_buffer(&mut self) {
        while self.buffer.is_empty() && !self.finished {
            let mut decoded = Audio::empty();
            match self.decoder.receive_frame(&mut decoded) {
                Ok(()) => {
                    self.ingest_frame(&decoded);
                    continue;
                }
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::util::error::EAGAIN => {}
                Err(_) => {}
            }

            let mut packet = ffmpeg::Packet::empty();
            match packet.read(&mut self.ictx) {
                Ok(()) => {
                    if packet.stream() == self.stream_index {
                        let _ = self.decoder.send_packet(&packet);
                    }
                }
                Err(_) => {
                    let _ = self.decoder.send_eof();
                    self.drain_decoder();
                    self.finished = true;
                }
            }
        }
    }
}

impl Iterator for FfmpegAudioSource {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        if self.buffer.is_empty() {
            self.fill_buffer();
        }
        self.buffer.pop_front()
    }
}

impl Source for FfmpegAudioSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> ChannelCount {
        ChannelCount::new(self.out_channels).unwrap_or(ChannelCount::new(2).unwrap())
    }
    fn sample_rate(&self) -> SampleRate {
        SampleRate::new(self.out_rate).unwrap_or(SampleRate::new(44_100).unwrap())
    }
    fn total_duration(&self) -> Option<Duration> {
        self.duration
    }
}

struct TapSource<S> {
    inner: S,
    tap: SampleBuffer,
}

impl<S> TapSource<S>
where
    S: Source<Item = f32>,
{
    fn new(inner: S, tap: SampleBuffer) -> Self {
        tap.set_format(inner.channels().get(), inner.sample_rate().get());
        Self { inner, tap }
    }
}

impl<S> Iterator for TapSource<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        let v = self.inner.next()?;
        self.tap.push(v);
        Some(v)
    }
}

impl<S> Source for TapSource<S>
where
    S: Source<Item = f32>,
{
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }
    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }
    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

/// Open an ffmpeg input, muting libav's chatty stderr warnings first.
fn open_input(path: &Path) -> Result<ffmpeg::format::context::Input> {
    ffmpeg::init().ok();
    ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Fatal);
    ffmpeg::format::input(&path.to_path_buf())
        .with_context(|| format!("opening {}", path.display()))
}

/// One selectable audio track inside a container, paired with the ffmpeg
/// stream index needed to decode it.
#[derive(Clone)]
struct AudioTrackInfo {
    stream_index: usize,
    label: String,
}

/// Enumerate every audio stream in `path`, returning the tracks (in container
/// order) and the index — into the returned vector — of the default ("best")
/// track ffmpeg would otherwise pick. Returns an empty list on any failure.
fn list_audio_tracks(path: &Path) -> (Vec<AudioTrackInfo>, usize) {
    let Ok(ictx) = open_input(path) else {
        return (Vec::new(), 0);
    };
    let best_index = ictx.streams().best(MediaType::Audio).map(|s| s.index());
    let mut tracks: Vec<AudioTrackInfo> = Vec::new();
    let mut default_idx = 0;
    for stream in ictx.streams() {
        if stream.parameters().medium() != MediaType::Audio {
            continue;
        }
        let idx = stream.index();
        if Some(idx) == best_index {
            default_idx = tracks.len();
        }
        let label = audio_track_label(&stream, tracks.len() + 1);
        tracks.push(AudioTrackInfo {
            stream_index: idx,
            label,
        });
    }
    (tracks, default_idx)
}

/// Build a human label for an audio stream from its language/title metadata,
/// suffixed with a channel-layout hint (e.g. "English (5.1)").
fn audio_track_label(stream: &ffmpeg::format::stream::Stream<'_>, n: usize) -> String {
    let meta = stream.metadata();
    let title = meta
        .get("title")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let language = meta
        .get("language")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "und");
    let lang_name = language.as_deref().map(language_display_name);

    let base = match lang_name {
        Some(name) => name,
        None => match title {
            Some(t) => t,
            None => format!("Track {n}"),
        },
    };
    match channel_desc(stream) {
        Some(c) => format!("{base} ({c})"),
        None => base,
    }
}

/// Best-effort channel-layout descriptor ("mono", "stereo", "5.1", …).
fn channel_desc(stream: &ffmpeg::format::stream::Stream<'_>) -> Option<String> {
    let ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters()).ok()?;
    let decoder = ctx.decoder().audio().ok()?;
    Some(match decoder.channels() {
        0 => return None,
        1 => "mono".to_string(),
        2 => "stereo".to_string(),
        6 => "5.1".to_string(),
        8 => "7.1".to_string(),
        c => format!("{c}ch"),
    })
}

pub struct AudioPlayer {
    sink: MixerDeviceSink,
    player: Player,
    tap: SampleBuffer,
    volume: f32,
    current_path: Option<PathBuf>,
    /// Audio tracks of the current file.
    audio_tracks: Vec<AudioTrackInfo>,
    /// Index into `audio_tracks` of the track currently being decoded.
    active_audio_track: usize,
}

impl AudioPlayer {
    pub fn new() -> Result<Self> {
        let mut sink = DeviceSinkBuilder::open_default_sink()
            .context("failed to open default audio output")?;
        sink.log_on_drop(false);
        let player = Player::connect_new(sink.mixer());
        let tap = SampleBuffer::new();
        Ok(Self {
            sink,
            player,
            tap,
            volume: 1.0,
            current_path: None,
            audio_tracks: Vec::new(),
            active_audio_track: 0,
        })
    }

    /// Open the audio of a video file on the currently selected track, falling
    /// back to ffmpeg's "best" stream when no track list is available.
    fn open_video_audio(&self, path: &Path) -> Result<FfmpegAudioSource> {
        match self.audio_tracks.get(self.active_audio_track) {
            Some(t) => FfmpegAudioSource::open_stream(path, t.stream_index),
            None => FfmpegAudioSource::open(path),
        }
    }

    /// Begin playing the audio of `path` (a video container). Returns the
    /// stream duration if known. Files with no audio stream are tolerated:
    /// playback simply produces no sound and the master clock stays at zero.
    pub fn play_file(&mut self, path: &Path) -> Result<Option<Duration>> {
        self.player.stop();
        self.player = Player::connect_new(self.sink.mixer());
        self.player.set_volume(self.volume);
        self.tap.reset();
        self.current_path = Some(path.to_path_buf());
        self.audio_tracks.clear();
        self.active_audio_track = 0;

        let (tracks, default_idx) = list_audio_tracks(path);
        self.audio_tracks = tracks;
        self.active_audio_track = default_idx;

        let total = match self.open_video_audio(path) {
            Ok(source) => {
                let total = source.total_duration();
                let tapped = TapSource::new(source, self.tap.clone());
                self.player.append(tapped);
                total
            }
            Err(e) => {
                // No audio stream (or it failed to open) — keep going silently.
                log::info!("no playable audio for {}: {e}", path.display());
                None
            }
        };
        self.player.play();
        Ok(total)
    }

    fn seek_to(&mut self, path: &Path, target: Duration) -> Result<()> {
        let was_paused = self.player.is_paused();
        self.player.stop();
        self.player = Player::connect_new(self.sink.mixer());
        self.player.set_volume(self.volume);

        self.tap.reset();
        self.tap.set_base_offset(target);

        if let Ok(mut source) = self.open_video_audio(path) {
            source.seek(target)?;
            let tapped = TapSource::new(source, self.tap.clone());
            self.player.append(tapped);
        }

        if was_paused {
            self.player.pause();
        } else {
            self.player.play();
        }
        Ok(())
    }

    pub fn toggle_pause(&self) {
        if self.player.is_paused() {
            self.player.play();
        } else {
            self.player.pause();
        }
    }

    /// Pause/resume the sink directly (independent of the user's play/pause
    /// state). Used to hold audio still while the video catches up after a seek,
    /// so the master clock can't run ahead of the decoded picture.
    pub fn set_sink_paused(&self, paused: bool) {
        if paused {
            self.player.pause();
        } else {
            self.player.play();
        }
    }

    pub fn seek_relative(&mut self, delta_secs: f64, total: Option<Duration>) -> Result<()> {
        let Some(path) = self.current_path.clone() else {
            return Ok(());
        };
        let cur = self.tap.position().as_secs_f64();
        let mut target_secs = (cur + delta_secs).max(0.0);
        if let Some(t) = total {
            let max = t.as_secs_f64();
            if max > 0.0 && target_secs > max - 0.05 {
                target_secs = (max - 0.05).max(0.0);
            }
        }
        self.seek_to(&path, Duration::from_secs_f64(target_secs))
    }

    pub fn position(&self) -> Duration {
        self.tap.position()
    }

    pub fn audio_tracks(&self) -> Vec<String> {
        self.audio_tracks.iter().map(|t| t.label.clone()).collect()
    }

    pub fn active_audio_track(&self) -> Option<usize> {
        if self.audio_tracks.is_empty() {
            None
        } else {
            Some(self.active_audio_track)
        }
    }

    /// Switch to a different audio track, re-decoding from the current playback
    /// position so picture and sound stay put.
    pub fn set_audio_track(&mut self, idx: usize) -> Result<()> {
        let Some(path) = self.current_path.clone() else {
            return Ok(());
        };
        if idx >= self.audio_tracks.len() {
            return Ok(());
        }
        self.active_audio_track = idx;
        let target = self.tap.position();
        let was_paused = self.player.is_paused();
        self.player.stop();
        self.player = Player::connect_new(self.sink.mixer());
        self.player.set_volume(self.volume);
        self.tap.reset();
        self.tap.set_base_offset(target);

        if let Ok(mut source) = self.open_video_audio(&path) {
            source.seek(target)?;
            let tapped = TapSource::new(source, self.tap.clone());
            self.player.append(tapped);
        }

        if was_paused {
            self.player.pause();
        } else {
            self.player.play();
        }
        Ok(())
    }

    /// Best-effort audio output latency from the negotiated buffer.
    pub fn output_buffer_latency(&self) -> Duration {
        let cfg = self.sink.config();
        let rate = cfg.sample_rate().get().max(1) as f64;
        let frames = match cfg.buffer_size() {
            rodio::cpal::BufferSize::Fixed(n) => *n as f64,
            rodio::cpal::BufferSize::Default => rate * 0.050,
        };
        Duration::from_secs_f64(frames / rate)
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.player.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Set `LV_SAMPLE_VIDEO` to a multi-track sample to run this. Ignored by
    /// default.
    #[test]
    #[ignore]
    fn enumerates_multiple_audio_tracks() {
        let path = std::env::var("LV_SAMPLE_VIDEO").expect("set LV_SAMPLE_VIDEO");
        let path = Path::new(&path);
        let (tracks, default_idx) = list_audio_tracks(path);
        assert!(tracks.len() >= 2, "expected >=2 audio tracks, got {}", tracks.len());
        assert!(default_idx < tracks.len());
        // Every enumerated stream must open and decode at least one sample.
        for t in &tracks {
            let mut src = FfmpegAudioSource::open_stream(path, t.stream_index)
                .unwrap_or_else(|e| panic!("opening track '{}' ({e})", t.label));
            assert!(src.next().is_some(), "track '{}' produced no samples", t.label);
        }
    }
}
