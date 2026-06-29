#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{env, error::Error, path::PathBuf, time::Duration};

mod app;
mod audio;
mod cache;
mod config;
mod decode;
mod formats;
mod geom;
mod gpu;
mod subtitles;
mod thumbnail;
mod types;
mod video;
mod video_state;
mod workers;

use crate::app::ImageViewerApp;
use crate::cache::clear_old_preload_cache;
use crate::decode::get_absolute_path;
use crate::gpu::Renderer;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use crate::windows::*;

// --- Main Entry Point ---
fn main() {
    // `/debug` forces verbose logs to stdout (primarily to diagnose startup
    // failures and HDR support). It's parsed straight off the args here, before
    // anything else can log, so the very first messages are captured too.
    let debug = env::args().any(|a| a.eq_ignore_ascii_case("/debug"));
    init_logging(debug);
    // The release build is `windows_subsystem = "windows"`, so there's no console
    // and stderr is invisible — without this a startup panic/error just flashes a
    // window and vanishes. Surface both the panic and the `run()` error paths via
    // a message box (and a log file) so failures are diagnosable.
    std::panic::set_hook(Box::new(|info| {
        report_fatal(&format!("LightningView crashed:\n\n{info}"));
    }));

    if let Err(e) = run() {
        let mut msg = format!("LightningView failed to start:\n\n{e}");
        let mut src = e.source();
        while let Some(s) = src {
            msg.push_str(&format!("\n  caused by: {s}"));
            src = s.source();
        }
        report_fatal(&msg);
        std::process::exit(1);
    }
}

/// Present a fatal error to the user (and persist it). The GUI-subsystem build has
/// no console, so a message box + temp-file log are the only visible channels.
fn report_fatal(msg: &str) {
    log::error!("{msg}");
    let log_path = std::env::temp_dir().join("lightningview-error.log");
    let _ = std::fs::write(&log_path, msg);
    let shown = format!("{msg}\n\n(A copy was saved to {})", log_path.display());
    let _ = sdl3::messagebox::show_simple_message_box(
        sdl3::messagebox::MessageBoxFlag::ERROR,
        "LightningView",
        &shown,
        None::<&sdl3::video::Window>,
    );
    #[cfg(not(target_os = "windows"))]
    eprintln!("{msg}");
}

/// Initialise logging. With `/debug` we force at least `debug` level to **stdout**
/// (overridable upward via `RUST_LOG`, e.g. `RUST_LOG=trace`); without it, the
/// normal `RUST_LOG`-driven behaviour is unchanged. On the Windows GUI-subsystem
/// build we first attach to the parent terminal's console so that stdout is
/// actually visible.
fn init_logging(debug: bool) {
    if debug {
        #[cfg(target_os = "windows")]
        crate::windows::attach_parent_console();
        env_logger::Builder::from_env(
            env_logger::Env::default().default_filter_or("debug"),
        )
        .target(env_logger::Target::Stdout)
        .format_timestamp_millis()
        .init();
        log::debug!("Debug logging enabled via /debug.");
    } else {
        env_logger::init();
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    clear_old_preload_cache();
    let args: Vec<String> = env::args().collect();

    // Separate option flags (any order) from the positional argument (the image /
    // video file, or a Windows `/register`-style subcommand). `/debug` is consumed
    // here too — it was already handled by `init_logging` in `main`.
    let mut is_fullscreen = true;
    let mut image_file_arg: Option<&String> = None;
    for arg in &args[1..] {
        if arg.eq_ignore_ascii_case("/windowed") {
            is_fullscreen = false;
        } else if arg.eq_ignore_ascii_case("/debug") {
            // Logging flag, handled in main(); ignore as a positional argument.
        } else if image_file_arg.is_none() {
            image_file_arg = Some(arg);
        }
    }

    let Some(image_file_arg) = image_file_arg else {
        println!("Usage: {} [/windowed] [/debug] <imagefile>", args[0]);
        return Ok(());
    };

    #[cfg(target_os = "windows")]
    {
        if image_file_arg.eq_ignore_ascii_case("/register") {
            return match register_urlhandler() {
                Ok(_) => {
                    println!("Success! Registered as image viewer.");
                    Ok(())
                }
                Err(err) => {
                    println!("Failed to register: {}", err);
                    Ok(())
                }
            };
        } else if image_file_arg.eq_ignore_ascii_case("/unregister") {
            unregister_urlhandler();
            println!("Unregistered as image viewer.");
            return Ok(());
        } else if image_file_arg.eq_ignore_ascii_case("/cleanup") {
            // Invoked by the installer (and available manually) to remove stale /
            // duplicate per-user registrations left by older versions.
            cleanup_registrations();
            println!("Cleaned up legacy/duplicate registrations.");
            return Ok(());
        }
    }

    let initial_path: PathBuf = get_absolute_path(image_file_arg)?;

    // Set the Wayland/X11 application id (window class) before video init.
    sdl3::hint::set("SDL_APP_ID", "lightningview");

    let sdl = sdl3::init()?;
    let video = sdl.video()?;
    let mut renderer = Renderer::new(&video, "Lightning View", 1280, 720, is_fullscreen)?;

    let mut app = ImageViewerApp::new(Some(initial_path), is_fullscreen, &renderer);

    let mut event_pump = sdl.event_pump()?;
    'running: loop {
        // When the app has work to do (video playing, animation, momentum, a
        // pending decode, an open dialog) we poll without blocking and let vsync
        // pace the loop. When idle we block on a timed wait to avoid spinning the
        // CPU — a freshly-decoded image still appears within the timeout.
        if app.is_active() {
            for event in event_pump.poll_iter() {
                app.handle_event(&event, &mut renderer);
            }
        } else if let Some(event) = event_pump.wait_event_timeout(Duration::from_millis(100)) {
            app.handle_event(&event, &mut renderer);
            for event in event_pump.poll_iter() {
                app.handle_event(&event, &mut renderer);
            }
        }

        if app.quit_requested() {
            break 'running;
        }

        app.update(&mut renderer);
        if let Err(e) = app.render(&mut renderer) {
            log::error!("render error: {e}");
        }
    }

    app.shutdown();
    Ok(())
}
