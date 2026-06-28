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
fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();
    clear_old_preload_cache();
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        println!("Usage: {} [/windowed] <imagefile>", args[0]);
        return Ok(());
    }

    let mut is_fullscreen = true;
    let mut image_file_arg = &args[1];

    if args[1].eq_ignore_ascii_case("/windowed") {
        if args.len() > 2 {
            is_fullscreen = false;
            image_file_arg = &args[2];
        } else {
            println!("Missing image file after /windowed");
            return Ok(());
        }
    }

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

        app.update(&renderer);
        if let Err(e) = app.render(&mut renderer) {
            log::error!("render error: {e}");
        }
    }

    app.shutdown();
    Ok(())
}
