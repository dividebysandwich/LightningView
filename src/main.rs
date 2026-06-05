#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use std::{env, error::Error, path::PathBuf};

mod app;
mod cache;
mod decode;
mod formats;
mod thumbnail;
mod types;
mod workers;

use crate::app::ImageViewerApp;
use crate::cache::clear_old_preload_cache;
use crate::decode::get_absolute_path;

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
        }
    }

    let initial_path: PathBuf = get_absolute_path(image_file_arg)?;

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0])
            .with_min_inner_size([300.0, 200.0])
	    .with_app_id("lightningview"),
        ..Default::default()
    };

    eframe::run_native(
        "Lightning View (egui)",
        native_options,
        Box::new(|cc| Ok(Box::new(ImageViewerApp::new(cc, Some(initial_path), is_fullscreen)))),
    )?;

    Ok(())
}
