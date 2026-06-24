//! Per-file video playback state: owns the decode thread, the audio player, the
//! subtitle set, and the current frame texture. The audio clock is the A/V
//! master; videos with no audio fall back to a wall clock. Switching files or
//! quitting drops this struct, which stops the decode thread (via
//! `VideoStream`'s Drop) and the rodio sink (via `AudioPlayer`'s Drop).

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use egui::{Context, TextureHandle, TextureOptions};

use crate::audio::AudioPlayer;
use crate::subtitles::{self, SubtitleSet};
use crate::video::{frame_to_color_image, VideoStream};

const OSD_DURATION: Duration = Duration::from_millis(2000);

pub struct VideoState {
    stream: VideoStream,
    /// `None` when no audio output device could be opened.
    audio: Option<AudioPlayer>,
    /// True when audio is the master clock (a device opened *and* the file has a
    /// playable audio stream). Otherwise the wall clock below drives playback.
    use_audio_clock: bool,
    subtitles: SubtitleSet,
    texture: Option<TextureHandle>,
    pub frame_size: [usize; 2],
    duration: Option<Duration>,
    av_offset_secs: f64,
    active_subtitle_track: Option<usize>,
    current_subtitle: Option<String>,
    paused: bool,

    // Wall-clock fallback (used only when `use_audio_clock` is false).
    wall_base_secs: f64,
    wall_started: Option<Instant>,

    osd: Option<(String, Instant)>,
}

impl VideoState {
    pub fn open(path: &Path, _ctx: &Context) -> Result<Self> {
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
            texture: None,
            frame_size,
            duration,
            av_offset_secs,
            active_subtitle_track: None,
            current_subtitle: None,
            paused: false,
            wall_base_secs: 0.0,
            wall_started: Some(Instant::now()),
            osd: None,
        })
    }

    /// Current playback time in seconds (the A/V master clock).
    fn clock_secs(&self) -> f64 {
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

    /// Pull the frame for the current clock and upload it; refresh the active
    /// subtitle line. Called once per UI frame.
    pub fn tick(&mut self, ctx: &Context) {
        let clock = self.clock_secs();
        if let Some(frame) = self.stream.frame_at(clock) {
            self.frame_size = [frame.width as usize, frame.height as usize];
            let img = frame_to_color_image(&frame);
            match &mut self.texture {
                Some(tex) => tex.set(img, TextureOptions::LINEAR),
                None => {
                    self.texture =
                        Some(ctx.load_texture("video_frame", img, TextureOptions::LINEAR));
                }
            }
        }
        self.current_subtitle = self
            .active_subtitle_track
            .and_then(|i| self.subtitles.cue_at(i, clock));
    }

    pub fn texture(&self) -> Option<&TextureHandle> {
        self.texture.as_ref()
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

    fn set_osd(&mut self, text: String) {
        self.osd = Some((text, Instant::now()));
    }

    pub fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        if let Some(a) = &self.audio {
            a.toggle_pause();
        }
        if !self.use_audio_clock {
            if self.paused {
                self.wall_base_secs = self.clock_secs();
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
            let target = (self.clock_secs() + delta_secs).clamp(0.0, max);
            self.stream.seek(Duration::from_secs_f64(target));
            self.wall_base_secs = target;
            if self.wall_started.is_some() {
                self.wall_started = Some(Instant::now());
            }
        }

        let secs = delta_secs.abs() as i64;
        let msg = if delta_secs < 0.0 {
            format!("\u{2212}{secs}s")
        } else {
            format!("+{secs}s")
        };
        self.set_osd(msg);
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
