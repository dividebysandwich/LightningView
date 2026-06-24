//! Subtitle support for video playback: the data model + pure SRT/VTT/ASS
//! parsers, plus native loading (sidecar `.srt`/`.vtt` discovery and
//! background embedded-track extraction via ffmpeg). Ported from sparkplayer
//! (sparkplayer-core `subtitles.rs` + sparkplayer-native `subtitles_native.rs`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use ffmpeg_next as ffmpeg;
use ffmpeg::codec::Id;
use ffmpeg::format::stream::Disposition;
use ffmpeg::media::Type;

#[derive(Debug, Clone)]
pub struct SubtitleCue {
    pub start_secs: f64,
    pub end_secs: f64,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct SubtitleTrack {
    pub label: String,
    /// ISO language code when known; retained for labeling/future track
    /// selection even though playback currently keys off the label.
    #[allow(dead_code)]
    pub language: Option<String>,
    pub cues: Vec<SubtitleCue>,
}

#[derive(Debug, Default)]
struct SubtitleInner {
    tracks: Mutex<Vec<SubtitleTrack>>,
    cancelled: AtomicBool,
}

/// Thread-safe handle to a (possibly still-loading) set of subtitle tracks.
/// Producers append tracks via [`SubtitleSet::extend`]; readers can query
/// whatever is available at any moment without blocking playback.
#[derive(Debug, Default, Clone)]
pub struct SubtitleSet {
    inner: Arc<SubtitleInner>,
}

impl SubtitleSet {
    pub fn track_count(&self) -> usize {
        self.inner.tracks.lock().map(|g| g.len()).unwrap_or(0)
    }

    pub fn track_label(&self, idx: usize) -> Option<String> {
        let guard = self.inner.tracks.lock().ok()?;
        guard.get(idx).map(|t| t.label.clone())
    }

    /// Append tracks with at least one cue.
    pub fn extend(&self, tracks: impl IntoIterator<Item = SubtitleTrack>) {
        if let Ok(mut guard) = self.inner.tracks.lock() {
            guard.extend(tracks.into_iter().filter(|t| !t.cues.is_empty()));
        }
    }

    pub fn cue_at(&self, track_idx: usize, secs: f64) -> Option<String> {
        let guard = self.inner.tracks.lock().ok()?;
        let track = guard.get(track_idx)?;
        if track.cues.is_empty() {
            return None;
        }
        let i = track.cues.partition_point(|c| c.start_secs <= secs);
        if i == 0 {
            return None;
        }
        let cue = &track.cues[i - 1];
        if cue.end_secs >= secs {
            Some(cue.text.clone())
        } else {
            None
        }
    }

    /// Signal any background loader to stop as soon as it can.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Relaxed)
    }
}

/// Load subtitles for a video: sidecar `.srt`/`.vtt` files synchronously, then
/// embedded text-subtitle tracks on a background thread.
pub fn load_for_video(video_path: &Path) -> SubtitleSet {
    let set = SubtitleSet::default();
    let sidecars = discover_sidecars(video_path);
    if !sidecars.is_empty() {
        set.extend(sidecars);
    }

    let path = video_path.to_path_buf();
    let set_t = set.clone();
    let _ = thread::Builder::new()
        .name("lightningview-subs".into())
        .spawn(move || {
            if set_t.is_cancelled() {
                return;
            }
            let cancelled = AtomicBool::new(false);
            let tracks = extract_embedded(&path, &set_t, &cancelled).unwrap_or_default();
            if set_t.is_cancelled() {
                return;
            }
            if !tracks.is_empty() {
                set_t.extend(tracks);
            }
        });

    set
}

fn extract_embedded(
    video_path: &Path,
    set: &SubtitleSet,
    cancelled: &AtomicBool,
) -> Option<Vec<SubtitleTrack>> {
    ffmpeg::init().ok();
    let mut ictx = ffmpeg::format::input(&video_path.to_path_buf()).ok()?;

    struct Pending {
        index: usize,
        label: String,
        language: Option<String>,
        time_base_num: f64,
        time_base_den: f64,
        decoder: ffmpeg::codec::decoder::Subtitle,
        cues: Vec<SubtitleCue>,
    }

    let mut pendings: Vec<Pending> = Vec::new();
    for stream in ictx.streams() {
        let params = stream.parameters();
        if params.medium() != Type::Subtitle {
            continue;
        }
        let codec_id = params.id();
        if !is_text_codec(codec_id) {
            continue;
        }
        let Ok(ctx) = ffmpeg::codec::context::Context::from_parameters(params) else {
            continue;
        };
        let Ok(decoder) = ctx.decoder().subtitle() else {
            continue;
        };
        let meta = stream.metadata();
        let title = meta
            .get("title")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let language = meta
            .get("language")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s != "und");
        let n = pendings.len() + 1;
        let label = build_label(language.as_deref(), title.as_deref(), stream.disposition(), n);
        let tb = stream.time_base();
        pendings.push(Pending {
            index: stream.index(),
            label,
            language,
            time_base_num: tb.numerator() as f64,
            time_base_den: tb.denominator() as f64,
            decoder,
            cues: Vec::new(),
        });
    }

    if pendings.is_empty() {
        return Some(Vec::new());
    }

    // Tell ffmpeg to drop everything that isn't one of our subtitle streams.
    let kept: std::collections::HashSet<usize> = pendings.iter().map(|p| p.index).collect();
    let stream_count = ictx.nb_streams() as usize;
    for i in 0..stream_count {
        if let Some(mut sm) = ictx.stream_mut(i) {
            if !kept.contains(&i) {
                unsafe {
                    (*sm.as_mut_ptr()).discard = ffmpeg::ffi::AVDiscard::AVDISCARD_ALL;
                }
            }
        }
    }

    for (stream, packet) in ictx.packets() {
        if cancelled.load(Ordering::Relaxed) || set.is_cancelled() {
            return None;
        }
        let idx = stream.index();
        let Some(pending) = pendings.iter_mut().find(|p| p.index == idx) else {
            continue;
        };
        let pts = packet.pts().unwrap_or(0);
        let packet_dur = packet.duration();
        let mut sub = ffmpeg::Subtitle::new();
        match pending.decoder.decode(&packet, &mut sub) {
            Ok(true) => {}
            _ => continue,
        }
        let base_secs = pts as f64 * pending.time_base_num / pending.time_base_den;
        let start_off_ms = sub.start() as f64;
        let end_off_ms = sub.end() as f64;
        let mut start_secs = base_secs + start_off_ms / 1000.0;
        let mut end_secs = base_secs + end_off_ms / 1000.0;
        if end_secs <= start_secs {
            let dur = if packet_dur > 0 {
                packet_dur as f64 * pending.time_base_num / pending.time_base_den
            } else {
                2.0
            };
            end_secs = start_secs + dur;
        }
        if start_secs < 0.0 {
            start_secs = 0.0;
        }

        for rect in sub.rects() {
            let text = match rect {
                ffmpeg::codec::subtitle::Rect::Text(t) => clean_html(t.get()),
                ffmpeg::codec::subtitle::Rect::Ass(a) => parse_ass_dialogue(a.get()),
                _ => continue,
            };
            let text = text.trim().to_string();
            if text.is_empty() {
                continue;
            }
            pending.cues.push(SubtitleCue {
                start_secs,
                end_secs,
                text,
            });
        }
    }

    let mut tracks: Vec<SubtitleTrack> = pendings
        .into_iter()
        .map(|p| {
            let mut cues = p.cues;
            cues.sort_by(|a, b| {
                a.start_secs
                    .partial_cmp(&b.start_secs)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            SubtitleTrack {
                label: p.label,
                language: p.language,
                cues,
            }
        })
        .collect();
    tracks.retain(|t| !t.cues.is_empty());
    Some(tracks)
}

fn build_label(
    language: Option<&str>,
    title: Option<&str>,
    disposition: Disposition,
    n: usize,
) -> String {
    let lang_name = language.map(language_display_name);
    let mut qualifiers: Vec<&str> = Vec::new();
    if disposition.contains(Disposition::FORCED) {
        qualifiers.push("forced");
    }
    if disposition.contains(Disposition::HEARING_IMPAIRED) {
        qualifiers.push("SDH");
    }
    if disposition.contains(Disposition::COMMENT) {
        qualifiers.push("commentary");
    }
    if let Some(t) = title {
        let lower = t.to_ascii_lowercase();
        if !qualifiers.contains(&"SDH") && (lower.contains("sdh") || lower.contains("hearing")) {
            qualifiers.push("SDH");
        }
        if !qualifiers.contains(&"forced") && lower.contains("forced") {
            qualifiers.push("forced");
        }
        if !qualifiers.contains(&"commentary") && lower.contains("comment") {
            qualifiers.push("commentary");
        }
    }
    let qual_suffix = if qualifiers.is_empty() {
        String::new()
    } else {
        format!(" ({})", qualifiers.join(", "))
    };
    match lang_name {
        Some(name) => format!("{name}{qual_suffix}"),
        None => {
            let title_is_meaningful = title
                .map(|t| t.contains(' ') || t.chars().any(|c| c.is_ascii_lowercase()))
                .unwrap_or(false);
            match title {
                Some(t) if title_is_meaningful => format!("{t}{qual_suffix}"),
                _ => format!("Track {n}{qual_suffix}"),
            }
        }
    }
}

fn is_text_codec(id: Id) -> bool {
    matches!(id, Id::SUBRIP | Id::ASS | Id::SSA | Id::MOV_TEXT | Id::WEBVTT)
}

fn discover_sidecars(video_path: &Path) -> Vec<SubtitleTrack> {
    let mut out = Vec::new();
    let Some(dir) = video_path.parent() else {
        return out;
    };
    let Some(video_stem) = video_path.file_stem().and_then(|s| s.to_str()) else {
        return out;
    };
    let video_stem_l = video_stem.to_ascii_lowercase();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut candidates: Vec<(PathBuf, String, String)> = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if !path.is_file() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        let ext_l = ext.to_ascii_lowercase();
        if ext_l != "srt" && ext_l != "vtt" {
            continue;
        }
        let stem_l = stem.to_ascii_lowercase();
        let lang_suffix = if stem_l == video_stem_l {
            String::new()
        } else if let Some(rest) = stem_l.strip_prefix(&format!("{video_stem_l}.")) {
            rest.to_string()
        } else {
            continue;
        };
        candidates.push((path, lang_suffix, ext_l));
    }
    candidates.sort_by(|a, b| a.1.cmp(&b.1).then(a.2.cmp(&b.2)));

    for (path, lang, ext) in candidates {
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let text = decode_text(&bytes);
        let cues = match ext.as_str() {
            "srt" => parse_srt(&text),
            "vtt" => parse_vtt(&text),
            _ => continue,
        };
        if cues.is_empty() {
            continue;
        }
        let label = if lang.is_empty() {
            format!("sidecar ({ext})")
        } else {
            format!("{lang} (sidecar)")
        };
        let language = if lang.is_empty() { None } else { Some(lang) };
        out.push(SubtitleTrack {
            label,
            language,
            cues,
        });
    }
    out
}

/// Map ISO 639-1 / 639-2 / 639-3 codes to English language names. Falls back
/// to returning the code as-is for codes we don't know.
pub fn language_display_name(code: &str) -> String {
    let key = code.trim().to_ascii_lowercase();
    let name = match key.as_str() {
        "en" | "eng" => "English",
        "de" | "ger" | "deu" => "German",
        "fr" | "fre" | "fra" => "French",
        "es" | "spa" => "Spanish",
        "it" | "ita" => "Italian",
        "pt" | "por" => "Portuguese",
        "nl" | "dut" | "nld" => "Dutch",
        "sv" | "swe" => "Swedish",
        "no" | "nor" => "Norwegian",
        "da" | "dan" => "Danish",
        "fi" | "fin" => "Finnish",
        "is" | "ice" | "isl" => "Icelandic",
        "pl" | "pol" => "Polish",
        "cs" | "cze" | "ces" => "Czech",
        "sk" | "slo" | "slk" => "Slovak",
        "hu" | "hun" => "Hungarian",
        "ro" | "rum" | "ron" => "Romanian",
        "ru" | "rus" => "Russian",
        "uk" | "ukr" => "Ukrainian",
        "bg" | "bul" => "Bulgarian",
        "sr" | "srp" => "Serbian",
        "hr" | "hrv" => "Croatian",
        "sl" | "slv" => "Slovenian",
        "el" | "gre" | "ell" => "Greek",
        "tr" | "tur" => "Turkish",
        "he" | "heb" => "Hebrew",
        "ar" | "ara" => "Arabic",
        "fa" | "per" | "fas" => "Persian",
        "hi" | "hin" => "Hindi",
        "bn" | "ben" => "Bengali",
        "ur" | "urd" => "Urdu",
        "ta" | "tam" => "Tamil",
        "te" | "tel" => "Telugu",
        "th" | "tha" => "Thai",
        "vi" | "vie" => "Vietnamese",
        "id" | "ind" => "Indonesian",
        "ms" | "may" | "msa" => "Malay",
        "fil" | "tgl" => "Filipino",
        "zh" | "chi" | "zho" => "Chinese",
        "ja" | "jpn" => "Japanese",
        "ko" | "kor" => "Korean",
        "lat" | "la" => "Latin",
        _ => return code.to_string(),
    };
    name.to_string()
}

/// Decode subtitle bytes to text, stripping a UTF-8 BOM and tolerating invalid
/// sequences.
pub fn decode_text(bytes: &[u8]) -> String {
    let trimmed = if bytes.starts_with(b"\xEF\xBB\xBF") {
        &bytes[3..]
    } else {
        bytes
    };
    match std::str::from_utf8(trimmed) {
        Ok(s) => s.to_string(),
        Err(_) => String::from_utf8_lossy(trimmed).into_owned(),
    }
}

fn parse_timestamp(s: &str) -> Option<f64> {
    let s = s.trim();
    let (main, ms_str) = match s.rsplit_once(|c| c == ',' || c == '.') {
        Some((a, b)) => (a, b),
        None => (s, "0"),
    };
    let ms: f64 = ms_str.parse().ok()?;
    let parts: Vec<&str> = main.split(':').collect();
    let (h, m, sec) = match parts.as_slice() {
        [h, m, s] => (h.parse::<f64>().ok()?, m.parse::<f64>().ok()?, s.parse::<f64>().ok()?),
        [m, s] => (0.0, m.parse::<f64>().ok()?, s.parse::<f64>().ok()?),
        _ => return None,
    };
    Some(h * 3600.0 + m * 60.0 + sec + ms / 1000.0)
}

pub fn parse_srt(text: &str) -> Vec<SubtitleCue> {
    parse_srt_or_vtt(text, false)
}

pub fn parse_vtt(text: &str) -> Vec<SubtitleCue> {
    parse_srt_or_vtt(text, true)
}

fn parse_srt_or_vtt(text: &str, is_vtt: bool) -> Vec<SubtitleCue> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = Vec::new();
    let mut block: Vec<&str> = Vec::new();
    for line in normalized.split('\n').chain(std::iter::once("")) {
        if line.is_empty() {
            if !block.is_empty() {
                if let Some(cue) = parse_block(&block, is_vtt) {
                    out.push(cue);
                }
                block.clear();
            }
            continue;
        }
        block.push(line);
    }
    out
}

fn parse_block(block: &[&str], is_vtt: bool) -> Option<SubtitleCue> {
    let mut i = 0;
    if is_vtt {
        let head = block[0].trim();
        if head.starts_with("WEBVTT") || head == "NOTE" || head.starts_with("NOTE ")
            || head == "STYLE" || head == "REGION"
        {
            return None;
        }
    }
    if !block[i].contains("-->") {
        i += 1;
        if i >= block.len() {
            return None;
        }
    }
    if !block[i].contains("-->") {
        return None;
    }
    let timeline = block[i];
    let (start_s, rest) = timeline.split_once("-->")?;
    let end_s = rest.trim().split_whitespace().next()?;
    let start_secs = parse_timestamp(start_s)?;
    let end_secs = parse_timestamp(end_s)?;
    i += 1;
    if i >= block.len() {
        return None;
    }
    let payload = block[i..].join("\n");
    let text = clean_html(&payload).trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(SubtitleCue {
        start_secs,
        end_secs,
        text,
    })
}

/// Strip simple `<...>` HTML tags. Used for SRT/VTT and to clean up Text-rect
/// subtitle payloads which may contain `<i>` etc.
pub fn clean_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

/// Parse an ASS dialogue payload (as emitted in `AVSubtitleRect.ass`) and
/// return the plain visible text.
pub fn parse_ass_dialogue(s: &str) -> String {
    let trimmed = s.trim();
    let mut lines = Vec::new();
    for raw in trimmed.split('\n') {
        let line = raw.trim().trim_start_matches('\r');
        if line.is_empty() {
            continue;
        }
        let payload = if let Some(rest) = line.strip_prefix("Dialogue:") {
            nth_comma_tail(rest.trim_start(), 9)
        } else {
            nth_comma_tail(line, 8)
        };
        if let Some(text) = payload {
            lines.push(strip_ass_overrides(text));
        }
    }
    lines.join("\n")
}

fn nth_comma_tail(s: &str, n: usize) -> Option<&str> {
    let mut count = 0;
    for (i, ch) in s.char_indices() {
        if ch == ',' {
            count += 1;
            if count == n {
                return Some(&s[i + 1..]);
            }
        }
    }
    None
}

fn strip_ass_overrides(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_brace = false;
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '{' => in_brace = true,
            '}' if in_brace => in_brace = false,
            _ if in_brace => {}
            '\\' => match chars.peek() {
                Some('N') | Some('n') => {
                    chars.next();
                    out.push('\n');
                }
                Some('h') => {
                    chars.next();
                    out.push(' ');
                }
                Some(_) | None => out.push(ch),
            },
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_basic() {
        let text = "1\n00:00:01,000 --> 00:00:02,500\nHello world\n\n2\n00:00:03,000 --> 00:00:04,000\n<i>Second</i> line\n";
        let cues = parse_srt(text);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].start_secs, 1.0);
        assert_eq!(cues[0].end_secs, 2.5);
        assert_eq!(cues[0].text, "Hello world");
        assert_eq!(cues[1].text, "Second line");
    }

    #[test]
    fn vtt_basic() {
        let text = "WEBVTT\n\nNOTE this is a note\n\n00:00:01.000 --> 00:00:02.000 align:start\nFirst\n\ncue-id\n00:00:03.000 --> 00:00:04.000\nSecond";
        let cues = parse_vtt(text);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "First");
        assert_eq!(cues[1].text, "Second");
    }

    #[test]
    fn cue_at_binary_search() {
        let set = SubtitleSet::default();
        set.extend(vec![SubtitleTrack {
            label: "t".into(),
            language: None,
            cues: vec![
                SubtitleCue { start_secs: 1.0, end_secs: 2.0, text: "a".into() },
                SubtitleCue { start_secs: 3.0, end_secs: 4.0, text: "b".into() },
            ],
        }]);
        assert_eq!(set.cue_at(0, 0.5), None);
        assert_eq!(set.cue_at(0, 1.5).as_deref(), Some("a"));
        assert_eq!(set.cue_at(0, 2.5), None);
        assert_eq!(set.cue_at(0, 3.5).as_deref(), Some("b"));
    }

    #[test]
    fn ass_strip() {
        let s = "0,0,Default,,0,0,0,,{\\an8}Hello\\Nworld";
        assert_eq!(parse_ass_dialogue(s), "Hello\nworld");
    }

    /// Set `LV_SAMPLE_VIDEO` to a sample with an embedded text subtitle track.
    /// Ignored by default.
    #[test]
    #[ignore]
    fn extracts_embedded_subtitles() {
        let path = std::env::var("LV_SAMPLE_VIDEO").expect("set LV_SAMPLE_VIDEO");
        let set = load_for_video(Path::new(&path));
        // Embedded extraction runs on a background thread; poll briefly.
        let mut count = 0;
        for _ in 0..200 {
            count = set.track_count();
            if count > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(count > 0, "expected at least one subtitle track");
        // The generated sample shows a cue around t=2s.
        let cue = set.cue_at(0, 2.0);
        assert!(cue.is_some(), "expected an active cue at t=2s");
    }
}
