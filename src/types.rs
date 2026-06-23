// --- Advanced Data Structures for Tiled Viewing ---
use egui::{ColorImage, TextureHandle};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{Receiver, Sender},
    },
    time::{Duration, Instant},
};

pub struct DisplayableImage {
    /// The full-resolution original image, kept in CPU memory.
    pub full_res_image: ColorImage,
    /// A single, downscaled texture for fast previews when zoomed out.
    pub preview_texture: TextureHandle,
    /// Cache for detail tiles to avoid re-uploading them to the GPU every frame.
    pub tile_cache: HashMap<(usize, usize), (TextureHandle, [usize; 2])>,
    /// Does this image actually need tiling, or is it small enough to fit on the GPU?
    pub needs_tiling: bool,
    /// Animation playback state for animated images (e.g. GIFs). `None` for stills.
    pub animation: Option<Animation>,
}

/// Request sent to the long-lived full-res decoder worker.
pub struct FullResRequest {
    pub path: PathBuf,
    pub preview_width: u32,
}

/// Reply produced by the full-res worker for the UI to consume.
pub struct FullResReply {
    pub path: PathBuf,
    pub preview_width: u32,
    /// True for the fast scaled-preview reply that precedes the full-resolution
    /// decode. The UI displays it but keeps `full_res_pending` set so the sharp
    /// full-res image still swaps in behind it.
    pub is_preview: bool,
    pub result: Result<LoadedImage, String>,
}

/// Single long-lived worker that decodes the foreground full-res image.
/// Rapid navigation queues many requests; the worker drains intermediate ones
/// and only decodes the most recently requested image, so the CPU isn't split
/// across N stale decodes when the user settles on a frame.
pub struct FullResWorker {
    pub tx: Sender<FullResRequest>,
    pub rx: Receiver<FullResReply>,
}

/// Shared control flag for the long-lived bulk preload workers.
/// `pause` is set while a foreground full-res decode is in flight so the OS scheduler
/// gives the foreground thread effectively all the CPU — without this the user's
/// "switch to next image" decode shares cores with N-1 bulk decoders and stalls.
pub struct PreloadState {
    pub shutdown: AtomicBool,
    pub pause: AtomicBool,
    pub gate: Arc<MemoryGate>,
}

/// Memory-aware admission control for image decoders. Holds a counter of how
/// many decodes are currently running and a sysinfo handle for checking RAM.
///
/// Rapid navigation can otherwise spawn many concurrent decoders (each peaking
/// hundreds of MB for large RAW/FITS files) and OOM the box — so before either
/// the foreground dispatcher or a bulk-preload worker begins a decode they must
/// `try_acquire()` a slot from the gate, which adapts to available memory.
pub struct MemoryGate {
    active: AtomicUsize,
    system: Mutex<sysinfo::System>,
}

impl MemoryGate {
    pub fn new() -> Self {
        Self {
            active: AtomicUsize::new(0),
            system: Mutex::new(sysinfo::System::new()),
        }
    }

    /// Reserve a decode slot if memory permits. Returns false when the system
    /// is too tight to safely start another decode; callers should sleep and
    /// retry.
    pub fn try_acquire(&self) -> bool {
        loop {
            let active = self.active.load(Ordering::Acquire);
            let max = self.max_concurrent();
            if active >= max {
                return false;
            }
            if self
                .active
                .compare_exchange(active, active + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }

    pub fn release(&self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }

    /// How many concurrent decodes we'll tolerate right now. Always >= 1 so the
    /// app never deadlocks even when memory is critically low; on a low-memory
    /// system this collapses to "one decode at a time", restoring the previous
    /// non-preempting behavior.
    fn max_concurrent(&self) -> usize {
        let Ok(mut sys) = self.system.lock() else { return 1 };
        sys.refresh_memory();
        let available = sys.available_memory();
        let total = sys.total_memory();
        drop(sys);

        // Reserve a safety margin so we don't push the OS into swap.
        let safety = (total / 10).max(512 * 1024 * 1024);
        let usable = available.saturating_sub(safety);
        // RAW/FITS decoding peaks at ~300 MB for typical files; use that as a
        // budget per concurrent decode.
        const PER_DECODE_BYTES: u64 = 300 * 1024 * 1024;
        let by_memory = (usable / PER_DECODE_BYTES) as usize;
        // Cap at a reasonable ceiling so we don't spin up dozens even on a
        // workstation with hundreds of GB.
        by_memory.clamp(1, 8)
    }
}

/// One frame of an animated image (e.g. GIF), already composited to the full
/// canvas, plus how long it should be shown before advancing.
pub struct AnimationFrame {
    pub image: ColorImage,
    pub delay: Duration,
}

/// Playback state for an animated image. All frames are decoded up front; the UI
/// advances `current` based on wall-clock time and re-uploads the active frame to
/// the displayable image's `preview_texture` whenever it changes.
pub struct Animation {
    pub frames: Vec<AnimationFrame>,
    pub current: usize,
    /// When the currently-displayed frame began showing.
    pub frame_started: Instant,
}

// Simplified enum for loaded image data before GPU upload
pub enum LoadedImage {
    Static(ColorImage),
    Animated(Vec<AnimationFrame>),
}
