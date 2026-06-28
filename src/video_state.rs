//! Per-file video playback state: owns the decode thread, the audio player, the
//! subtitle set, and the current frame texture. The audio clock is the A/V
//! master; videos with no audio fall back to a wall clock. Switching files or
//! quitting drops this struct, which stops the decode thread (via
//! `VideoStream`'s Drop) and the rodio sink (via `AudioPlayer`'s Drop).

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::audio::AudioPlayer;
use crate::gpu::{GpuTexture, Renderer};
use crate::subtitles::{self, SubtitleSet};
use crate::video::{ColorInfo, FrameDepth, VideoStream};

const OSD_DURATION: Duration = Duration::from_millis(2000);

/// How long the on-screen controls (seek bar + time readout) stay visible after
/// the last interaction (seek, pause/resume, track change) before fading out.
const CONTROLS_DURATION: Duration = Duration::from_millis(3500);

/// Time constant for the master-clock low-pass filter. Long enough to smooth a
/// coarse audio-callback staircase down to a few ms of ripple, short enough to
/// settle within a couple of seconds after the initial sync / a seek.
const CLOCK_SMOOTH_TAU_SECS: f64 = 1.5;

pub struct VideoState {
    stream: VideoStream,
    /// `None` when no audio output device could be opened.
    audio: Option<AudioPlayer>,
    /// True when audio is the master clock (a device opened *and* the file has a
    /// playable audio stream). Otherwise the wall clock below drives playback.
    use_audio_clock: bool,
    subtitles: SubtitleSet,
    /// Current frame's YUV plane textures (luma, interleaved chroma). Replaced
    /// each time a new frame is uploaded; the GPU video shader converts to RGB.
    y_tex: Option<GpuTexture>,
    uv_tex: Option<GpuTexture>,
    /// Colour description of the most recently uploaded frame (SDR vs HDR, etc.).
    color: Option<ColorInfo>,
    pub frame_size: [usize; 2],
    duration: Option<Duration>,
    av_offset_secs: f64,
    active_subtitle_track: Option<usize>,
    current_subtitle: Option<String>,
    paused: bool,

    // Smoothed master clock. The raw audio clock advances in bursts — one step
    // per output-buffer callback — so reading it directly to pick the video
    // frame makes the picture hold still then skip several frames at the
    // callback rate. Instead we advance this clock by real elapsed time and
    // gently correct it toward the raw audio clock, tracking audio without
    // reproducing its staircase (the judder VLC/jellyfin avoid the same way).
    smooth_clock_secs: f64,
    clock_initialized: bool,
    last_clock_tick: Option<Instant>,

    // Wall-clock fallback (used only when `use_audio_clock` is false).
    wall_base_secs: f64,
    wall_started: Option<Instant>,

    osd: Option<(String, Instant)>,
    /// When the seek bar / time HUD was last (re)shown. Refreshed on every
    /// interaction; the HUD is drawn only while within `CONTROLS_DURATION` of it.
    controls_shown_at: Instant,
}

impl VideoState {
    pub fn open(path: &Path) -> Result<Self> {
        let stream = VideoStream::open(path)?;
        let frame_size = [stream.width as usize, stream.height as usize];
        let duration = stream.duration;

        // An audio device is best-effort: video still plays without one.
        let mut audio = AudioPlayer::new().ok();
        if let Some(a) = audio.as_mut() {
            let _ = a.play_file(path);
        }
        let use_audio_clock = audio
            .as_ref()
            .map(|a| a.active_audio_track().is_some())
            .unwrap_or(false);

        let av_offset_secs = if use_audio_clock {
            audio
                .as_ref()
                .map(|a| a.output_buffer_latency().as_secs_f64() * 2.0 + 0.02)
                .unwrap_or(0.05)
                .max(0.05)
        } else {
            0.0
        };

        let subtitles = subtitles::load_for_video(path);

        Ok(Self {
            stream,
            audio,
            use_audio_clock,
            subtitles,
            y_tex: None,
            uv_tex: None,
            color: None,
            frame_size,
            duration,
            av_offset_secs,
            active_subtitle_track: None,
            current_subtitle: None,
            paused: false,
            smooth_clock_secs: 0.0,
            clock_initialized: false,
            last_clock_tick: None,
            wall_base_secs: 0.0,
            wall_started: Some(Instant::now()),
            osd: None,
            // Show the controls briefly when a video first opens, then fade out.
            controls_shown_at: Instant::now(),
        })
    }

    /// The unsmoothed master clock: the audio sink position (minus the A/V
    /// offset) when audio drives playback, else the wall-clock fallback.
    fn raw_clock_secs(&self) -> f64 {
        if self.use_audio_clock {
            if let Some(a) = &self.audio {
                return (a.position().as_secs_f64() - self.av_offset_secs).max(0.0);
            }
        }
        match self.wall_started {
            Some(t) => self.wall_base_secs + t.elapsed().as_secs_f64(),
            None => self.wall_base_secs,
        }
    }

    /// Advance the smoothed clock toward the raw clock and return it. The wall
    /// path is already smooth, so it passes through; the audio path is
    /// low-pass filtered to remove the per-callback staircase.
    fn advance_clock(&mut self) -> f64 {
        let raw = self.raw_clock_secs();

        // The wall clock is monotonic and smooth already; nothing to filter.
        if !self.use_audio_clock {
            self.smooth_clock_secs = raw;
            self.clock_initialized = true;
            return raw;
        }

        let now = Instant::now();
        let dt = self
            .last_clock_tick
            .map(|t| (now - t).as_secs_f64())
            .unwrap_or(0.0);
        self.last_clock_tick = Some(now);

        // Resync hard on the first tick, while paused, after a long stall, or on
        // any large discontinuity (a seek). Seeks are >=5s, far above 1s, while
        // a normal staircase step (one audio buffer) stays well under it.
        if !self.clock_initialized
            || self.paused
            || dt > 0.5
            || (raw - self.smooth_clock_secs).abs() > 1.0
        {
            self.smooth_clock_secs = raw;
            self.clock_initialized = true;
            return raw;
        }

        // Run forward in real time, then nudge toward audio with a slow
        // (time-constant based) correction. Because the raw clock is centered on
        // true playback, filtering its staircase out introduces no net A/V lag;
        // the long time constant attenuates even a coarse (hundreds-of-ms)
        // staircase to a few ms of ripple, well under one frame. Audio-vs-system
        // crystal drift is only ~ppm, so this slow correction tracks it easily.
        self.smooth_clock_secs += dt;
        let err = raw - self.smooth_clock_secs;
        let alpha = 1.0 - (-dt / CLOCK_SMOOTH_TAU_SECS).exp();
        self.smooth_clock_secs += err * alpha;
        self.smooth_clock_secs
    }

    /// Pull the frame for the current clock and upload it; refresh the active
    /// subtitle line. Called once per UI frame.
    pub fn tick(&mut self, renderer: &Renderer) {
        let clock = self.advance_clock();
        if let Some(frame) = self.stream.frame_at(clock) {
            self.frame_size = [frame.width as usize, frame.height as usize];
            self.color = Some(frame.color);
            // Upload the two YUV planes as GPU textures (R8/R8G8 for 8-bit NV12,
            // R16/R16G16 for 10-bit P010). The previous frame's textures are freed
            // when these assignments drop them.
            let (y, uv) = match frame.depth {
                FrameDepth::Eight => (
                    renderer.upload_r8(frame.width, frame.height, &frame.y_plane),
                    renderer.upload_r8g8(frame.chroma_width, frame.chroma_height, &frame.uv_plane),
                ),
                FrameDepth::Ten => (
                    renderer.upload_r16(frame.width, frame.height, &frame.y_plane),
                    renderer.upload_r16g16(frame.chroma_width, frame.chroma_height, &frame.uv_plane),
                ),
            };
            match (y, uv) {
                (Ok(y), Ok(uv)) => {
                    self.y_tex = Some(y);
                    self.uv_tex = Some(uv);
                }
                (e1, e2) => {
                    if let Err(e) = e1 {
                        log::warn!("Failed to upload video luma plane: {e}");
                    }
                    if let Err(e) = e2 {
                        log::warn!("Failed to upload video chroma plane: {e}");
                    }
                }
            }
        }
        self.current_subtitle = self
            .active_subtitle_track
            .and_then(|i| self.subtitles.cue_at(i, clock));
    }

    /// The current frame's (luma, chroma) plane textures, ready to draw via
    /// `Renderer::draw_video`. `None` until the first frame is decoded.
    pub fn planes(&self) -> Option<(&GpuTexture, &GpuTexture)> {
        Some((self.y_tex.as_ref()?, self.uv_tex.as_ref()?))
    }

    /// Colour description of the current frame (drives the video shader's colour
    /// pipeline). `None` until the first frame is decoded.
    pub fn video_color(&self) -> Option<ColorInfo> {
        self.color
    }

    /// Current playback position in seconds (the smoothed master clock).
    pub fn position_secs(&self) -> f64 {
        if self.clock_initialized {
            self.smooth_clock_secs
        } else {
            self.raw_clock_secs()
        }
    }

    /// Total duration in seconds, if the container reported one.
    pub fn duration_secs(&self) -> Option<f64> {
        self.duration.map(|d| d.as_secs_f64())
    }

    pub fn current_subtitle(&self) -> Option<&str> {
        self.current_subtitle.as_deref()
    }

    pub fn is_playing(&self) -> bool {
        !self.paused
    }

    pub fn osd_text(&self) -> Option<&str> {
        self.osd
            .as_ref()
            .filter(|(_, t)| t.elapsed() < OSD_DURATION)
            .map(|(s, _)| s.as_str())
    }

    /// Whether the seek bar / time HUD should currently be drawn. True for a few
    /// seconds after the last interaction (seek, pause/resume, track change).
    pub fn controls_visible(&self) -> bool {
        self.controls_shown_at.elapsed() < CONTROLS_DURATION
    }

    fn set_osd(&mut self, text: String) {
        self.osd = Some((text, Instant::now()));
        // Any action that surfaces an OSD message is an interaction, so re-show
        // the seek bar / time HUD alongside it.
        self.controls_shown_at = Instant::now();
    }

    pub fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        if let Some(a) = &self.audio {
            a.toggle_pause();
        }
        if !self.use_audio_clock {
            if self.paused {
                self.wall_base_secs = self.raw_clock_secs();
                self.wall_started = None;
            } else {
                self.wall_started = Some(Instant::now());
            }
        }
        let msg = if self.paused { "Paused" } else { "Playing" };
        self.set_osd(msg.to_string());
    }

    pub fn seek_relative(&mut self, delta_secs: f64) {
        let max = self
            .duration
            .map(|d| (d.as_secs_f64() - 0.2).max(0.0))
            .unwrap_or(f64::MAX);

        if self.use_audio_clock {
            let pos = self
                .audio
                .as_ref()
                .map(|a| a.position().as_secs_f64())
                .unwrap_or(0.0);
            let target = (pos + delta_secs).clamp(0.0, max);
            self.stream.seek(Duration::from_secs_f64(target));
            if let Some(a) = self.audio.as_mut() {
                let _ = a.seek_relative(delta_secs, self.duration);
            }
        } else {
            let target = (self.raw_clock_secs() + delta_secs).clamp(0.0, max);
            self.stream.seek(Duration::from_secs_f64(target));
            self.wall_base_secs = target;
            if self.wall_started.is_some() {
                self.wall_started = Some(Instant::now());
            }
        }

        let sign = if delta_secs < 0.0 { "\u{2212}" } else { "+" };
        let mag = delta_secs.abs() as i64;
        let label = if mag >= 60 && mag % 60 == 0 {
            format!("{}m", mag / 60)
        } else {
            format!("{mag}s")
        };
        self.set_osd(format!("{sign}{label}"));
    }

    pub fn cycle_audio_track(&mut self) {
        if self.audio.is_none() {
            self.set_osd("No audio output".to_string());
            return;
        }
        let tracks = self.audio.as_ref().unwrap().audio_tracks();
        if tracks.len() < 2 {
            self.set_osd("Only one audio track".to_string());
            return;
        }
        let cur = self.audio.as_ref().unwrap().active_audio_track().unwrap_or(0);
        let next = (cur + 1) % tracks.len();
        let res = self.audio.as_mut().unwrap().set_audio_track(next);
        let msg = match res {
            Ok(()) => format!("Audio: {}", tracks[next]),
            Err(e) => format!("Audio switch failed: {e}"),
        };
        self.set_osd(msg);
    }

    pub fn cycle_subtitle_track(&mut self) {
        let count = self.subtitles.track_count();
        if count == 0 {
            self.active_subtitle_track = None;
            self.current_subtitle = None;
            self.set_osd("No subtitles available (still loading?)".to_string());
            return;
        }
        // Cycle: Off -> Track 0 -> Track 1 -> ... -> Off
        let next = match self.active_subtitle_track {
            None => Some(0),
            Some(i) if i + 1 < count => Some(i + 1),
            Some(_) => None,
        };
        self.active_subtitle_track = next;
        self.current_subtitle = None;
        let msg = match next {
            Some(i) => format!(
                "Subtitles: {}",
                self.subtitles
                    .track_label(i)
                    .unwrap_or_else(|| format!("Track {}", i + 1))
            ),
            None => "Subtitles: off".to_string(),
        };
        self.set_osd(msg);
    }
}

impl Drop for VideoState {
    fn drop(&mut self) {
        // Stop the background subtitle extractor promptly; the video decode
        // thread and audio sink are stopped by their own Drop impls.
        self.subtitles.cancel();
    }
}
