use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{channel, Receiver, Sender},
    },
    thread,
    time::Duration,
};

use crate::cache::{preload_cache_path, save_preload_cache};
use crate::decode::{
    decode_image_data, decode_preview, jpeg_dimensions, load_full_for_worker, to_pixel_buf,
    PREVIEW_MAX_DIM,
};
use crate::types::{FullResReply, FullResRequest, FullResWorker, LoadedImage, MemoryGate, PreloadState};

/// Spawn N worker threads that share a queue of paths to preload.
/// Workers catch panics so a single bad file doesn't stall the rest of the queue,
/// and exit when `state.shutdown` is set or the queue drains. They run continuously
/// — the single latest-wins foreground decoder is enough on its own to keep the UI
/// responsive, and pausing the bulk pool around navigation just left cores idle.
pub fn spawn_preload_workers(state: Arc<PreloadState>, paths: Vec<PathBuf>) {
    // Use all available cores minus one (leave a core for the UI / foreground decoder).
    let n_workers = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .saturating_sub(1)
        .max(1);
    let queue = Arc::new(Mutex::new(VecDeque::from(paths)));
    log::debug!(
        "Bulk preload starting with {} workers ({} images)",
        n_workers,
        queue.lock().map(|q| q.len()).unwrap_or(0)
    );
    for _ in 0..n_workers {
        let state = state.clone();
        let queue = queue.clone();
        thread::spawn(move || preload_worker_loop(state, queue));
    }
}

fn preload_worker_loop(state: Arc<PreloadState>, queue: Arc<Mutex<VecDeque<PathBuf>>>) {
    loop {
        if state.shutdown.load(Ordering::Relaxed) {
            return;
        }
        // Yield to the foreground full-res decoder while it has work in flight.
        // Polling-with-sleep is intentional: a foreground decode is bounded by
        // FULL_RES_WATCHDOG, so we can't deadlock here.
        while state.pause.load(Ordering::Relaxed) {
            if state.shutdown.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let path = match queue.lock() {
            Ok(mut q) => q.pop_front(),
            Err(_) => return,
        };
        let Some(path) = path else { return };
        if preload_cache_path(&path).exists() {
            continue;
        }
        // Wait for a memory slot before allocating decoder buffers. This is what
        // keeps a roomful of N bulk workers from collectively running the box out
        // of RAM on a directory of large RAW/FITS files.
        loop {
            if state.shutdown.load(Ordering::Relaxed) {
                return;
            }
            // Continue yielding to foreground decodes that may have started
            // while we were idle.
            if state.pause.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            if state.gate.try_acquire() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        // catch_unwind: a panicking decoder (rare, but possible on malformed RAW/FITS)
        // must not take the worker thread down — the queue keeps draining.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            decode_image_data(&path)
        }));
        state.gate.release();
        match outcome {
            Ok(Ok(img)) => {
                if let Err(e) = save_preload_cache(&img, &path) {
                    log::warn!("Bulk preload save failed for {}: {}", path.display(), e);
                }
            }
            Ok(Err(e)) => log::debug!("Bulk preload decode failed for {}: {}", path.display(), e),
            Err(_) => log::warn!("Bulk preload decode panicked for {}", path.display()),
        }
    }
}

/// Spawn the full-res worker dispatcher. The dispatcher is a lightweight thread
/// that accepts requests and (memory permitting) immediately spawns a fresh
/// decode thread for each, so the user's *latest* navigation starts decoding
/// right away instead of waiting for a previously-started decode to finish. A
/// shared generation counter lets stale decodes (whose result the user no
/// longer cares about) drop their results when they eventually finish. The
/// `MemoryGate` caps concurrency so a burst of rapid navigation can't OOM the
/// system on large RAW/FITS files.
///
/// Replies are delivered over the mpsc channel; the SDL main loop drains them on
/// its next iteration (it keeps a short wait timeout while a decode is pending),
/// so no cross-thread repaint signal is needed.
pub fn spawn_full_res_worker(gate: Arc<MemoryGate>) -> FullResWorker {
    let (req_tx, req_rx) = channel::<FullResRequest>();
    let (reply_tx, reply_rx) = channel::<FullResReply>();
    thread::spawn(move || full_res_dispatcher_loop(req_rx, reply_tx, gate));
    FullResWorker { tx: req_tx, rx: reply_rx }
}

fn full_res_dispatcher_loop(
    req_rx: Receiver<FullResRequest>,
    reply_tx: Sender<FullResReply>,
    gate: Arc<MemoryGate>,
) {
    // Bumped on every accepted request; a decode thread only sends its reply if
    // its generation still matches when the decode finishes.
    let generation = Arc::new(AtomicU64::new(0));
    loop {
        let mut req = match req_rx.recv() {
            Ok(r) => r,
            Err(_) => return, // App dropped — exit.
        };
        // While we wait for a memory slot, keep draining newer requests so the
        // decode we eventually start is for the freshest navigation target.
        loop {
            while let Ok(newer) = req_rx.try_recv() {
                req = newer;
            }
            if gate.try_acquire() {
                break;
            }
            log::debug!("Foreground decoder waiting for memory slot");
            thread::sleep(Duration::from_millis(100));
        }
        let my_gen = generation.fetch_add(1, Ordering::Relaxed) + 1;
        let reply_tx = reply_tx.clone();
        let generation2 = generation.clone();
        let gate2 = gate.clone();
        thread::spawn(move || {
            // Phase 1: when nothing is on screen yet (no cache / embedded thumbnail),
            // emit a fast scaled preview so the user sees a crisp image almost
            // immediately. Only worth it for formats with a genuinely cheaper
            // reduced-resolution decode (JPEG's DCT scaling) — for others a
            // "preview" would just be a full decode we'd then repeat, so we skip
            // straight to the full-res decode below.
            let extension = req
                .path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            // ...and only when scaling actually saves work: for a JPEG already near
            // (or below) the preview size, the scaled decode returns full resolution,
            // so doing it *and* the full-res decode would decode the same file twice.
            // A cheap header read lets us skip the preview for those.
            let fast_preview = matches!(extension.as_str(), "jpg" | "jpeg")
                && jpeg_dimensions(&req.path)
                    .map(|(w, h)| w.max(h) as f32 > PREVIEW_MAX_DIM as f32 * 1.25)
                    .unwrap_or(false);
            // Width of whatever is currently displayed; used by the UI to preserve
            // the user's zoom across the preview→full-res swap.
            let mut shown_preview_width = req.preview_width;
            if req.preview_width == 0 && fast_preview {
                let preview = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    decode_preview(&req.path, PREVIEW_MAX_DIM)
                }))
                .ok()
                .and_then(|r| r.ok());
                if let Some(img) = preview {
                    if generation2.load(Ordering::Relaxed) == my_gen {
                        shown_preview_width = img.width();
                        let reply = FullResReply {
                            path: req.path.clone(),
                            preview_width: 0,
                            is_preview: true,
                            result: Ok(LoadedImage::Static(to_pixel_buf(img))),
                        };
                        let _ = reply_tx.send(reply);
                    }
                }
            }

            // Phase 2: full-resolution decode.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load_full_for_worker(&req.path)))
                .unwrap_or_else(|_| Err(format!("decoder panicked for {}", req.path.display())));
            gate2.release();
            // If a newer request has been accepted while we were decoding, this
            // result is no longer wanted. The full-res pixels are already decoded
            // though, so still persist the preload cache for a fast revisit.
            if generation2.load(Ordering::Relaxed) != my_gen {
                log::debug!("Discarding stale decode result for {}", req.path.display());
                if let Ok((_, Some(dyn_img))) = &outcome {
                    let _ = save_preload_cache(dyn_img, &req.path);
                }
                return;
            }
            let (result, dyn_for_cache) = match outcome {
                Ok((loaded, dyn_img)) => (Ok(loaded), dyn_img),
                Err(e) => (Err(e), None),
            };
            let reply = FullResReply {
                path: req.path.clone(),
                preview_width: shown_preview_width,
                is_preview: false,
                result,
            };
            let _ = reply_tx.send(reply);
            // Cache generation (resize + JPEG encode + disk write) now runs *after*
            // the UI already has its image, so it never delays first paint.
            if let Some(dyn_img) = dyn_for_cache {
                let _ = save_preload_cache(&dyn_img, &req.path);
            }
        });
    }
}
