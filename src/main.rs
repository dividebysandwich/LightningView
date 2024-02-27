//#[cfg(target_os = "windows")]
//#![windows_subsystem = "windows"]

use fltk::{app::{self, MouseWheel}, dialog, enums::Color, frame::Frame, image::SharedImage, prelude::*, window::Window};
use std::{env, error::Error, fs, path::{Path, PathBuf}};

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use crate::windows::*;
#[cfg(target_os = "unix")]
mod notwindows;
#[cfg(target_os = "unix")]
use crate::notwindows::*;
#[cfg(target_os = "macos")]
mod notwindows;
#[cfg(target_os = "macos")]
use crate::notwindows::*;

pub const FLTK_SUPPORTED_FORMATS: [&str; 10] = ["jpg", "jpeg", "png", "bmp", "gif", "svg", "ico", "pnm", "xbm", "xpm"];
pub const RAW_SUPPORTED_FORMATS: [&str; 23] = ["mrw", "arw", "srf", "sr2", "nef", "mef", "orf", "srw", "erf", "kdc", "dcs", "rw2", "raf", "dcr", "dng", "pef", "crw", "iiq", "3fr", "nrw", "mos", "cr2", "ari"];


fn load_and_display_image(original_image: &mut SharedImage, frame: &mut Frame, wind: &mut Window, path: &PathBuf, zoom_factor: &mut f64) {
    if let Ok(image) = load_image(&path.to_string_lossy()) {
        let mut new_image = image.clone();
        new_image.scale(wind.width(), wind.height(), true, true);
        frame.set_image(Some(new_image));
        *zoom_factor = 1.0;
        frame.set_pos(0, 0);
        wind.redraw();
        *original_image = image;
    }
}

fn get_absolute_path(filename: &str) -> PathBuf {
    let path = Path::new(filename);
    
    if path.is_absolute() {
        PathBuf::from(path)
    } else {
        let mut absolute_path = env::current_dir().expect("Failed to get the current working directory");
        absolute_path.push(filename);
        absolute_path
    }
}

fn load_raw(image_file: &str) -> Result<SharedImage, String> {
    println!("processing {}", image_file);

    match imagepipe::Pipeline::new_from_file(&image_file) {
        Ok(mut pipeline) => {
            match pipeline.output_8bit(Some(&imagepipe::Pipeline::new_cache(100000000))) {
                Ok(decoded) => {
                    match fltk::image::RgbImage::new(
                        &decoded.data,
                        decoded.width as i32,
                        decoded.height as i32,
                        fltk::enums::ColorDepth::Rgb8,
                    ) {
                        Ok(img) => {
                            match SharedImage::from_image(img) {
                                Ok(shared_img) => Ok(shared_img),
                                Err(err) => Err(format!("Error creating image: {}", err))
                            }
                        }
                        Err(err) => Err(format!("Processing for \"{}\" failed: {}", image_file, err.to_string())),
                    }
                }
                Err(err) => Err(format!("Processing for \"{}\" failed: {}", image_file, err.to_string()))
            }
        }
        Err(err) => Err(format!("Don't know how to load \"{}\": {}", image_file, err.to_string()))
    }
}

fn load_image(image_file: &str) -> Result<SharedImage, String> {
    if FLTK_SUPPORTED_FORMATS.iter().any(|&format| image_file.to_lowercase().ends_with(format)) {
        match SharedImage::load(image_file) {
            Ok(image) => Ok(image),
            Err(err) => Err(format!("Error loading image: {}", err)),
        }
    } else if RAW_SUPPORTED_FORMATS.iter().any(|&format| image_file.to_lowercase().ends_with(format)) {
        match load_raw(image_file) {
            Ok(image) => Ok(image),
            Err(err) => Err(format!("Error loading image: {}", err)),
        }
        
    } else {
        Err("Unsupported file format.".to_string())
    }
}

fn main() -> Result<(), Box<dyn Error>> {

    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        println!("Usage: {} <image_file>", args[0]);
        println!("To register as image viewer in Windows, run: {} /register", args[0]);
        println!("To unregister, run: {} /unregister", args[0]);
        std::process::exit(1);
    }

    let image_file = &args[1];

    #[cfg(target_os = "windows")]
    {
        if image_file.eq_ignore_ascii_case("/register") {
            match register_urlhandler() {
                Ok(_) => println!("Success! LightningView egistered as image viewer."),
                Err(err) => println!("Failed to register as image viewer: {}", err),
            }
            std::process::exit(0);
        } else if image_file.eq_ignore_ascii_case("/unregister") {
            unregister_urlhandler();
            println!("LightningView unregistered as image viewer.");
            std::process::exit(0);
        } 
    }
    
    let app = app::App::default();

    // Get the screen size
    let screen = app::screen_count(); // Get the number of screens
    let (screen_width, screen_height) = if screen > 0 {
        let screen = app::screen_xywh(0); // Get the work area of the primary screen
        (screen.2, screen.3)
    } else {
        (800, 600) // Default dimensions
    };

    let mut wind = Window::new(0, 0, screen_width, screen_height, "Lightning View");
    wind.set_color(Color::Black);
    wind.fullscreen(true);
    let mut frame = Frame::default_fill();

    let mut original_image = load_image(image_file)?;
    let mut image = original_image.clone();
    image.scale(wind.width(), wind.height(), true, true);

    frame.set_image(Some(image));
//    if image_file.ends_with(".gif") {
//        let flags = AnimGifImageFlags;
//        frame.set_image(Some(AnimGifImage::load(image_file, frame, flags)?));
//    }
    wind.end();
    wind.make_resizable(true);
    wind.show();

    // Add mouse wheel event handling for zooming
    let mut zoom_factor = 1.0;
    let mut pan_origin: Option<(i32, i32)> = None;
    let mut current_index = 0;
    let mut image_files: Vec<PathBuf> = Vec::new();

    println!("Image file: {}", image_file);

    let absolute_path = get_absolute_path(image_file);
    let parent_dir = absolute_path.parent().unwrap_or_else(|| {
        println!("Failed to get the parent directory.");
        std::process::exit(1);
    });

    println!("Parent dir: {:?}", parent_dir);


    if let Ok(entries) = fs::read_dir(parent_dir) {
        let mut all_supported_formats: Vec<&str> = Vec::new();
        all_supported_formats.extend(&FLTK_SUPPORTED_FORMATS);
        all_supported_formats.extend(&RAW_SUPPORTED_FORMATS);
        image_files = entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| {
                path.is_file()
                    && path != Path::new(image_file)
                    && all_supported_formats.iter().any(|&format| path.to_string_lossy().to_lowercase().ends_with(format) 
                )
            })
            .collect();
        image_files.sort();
        if let Some(index) = image_files.iter().position(|path| path == &absolute_path) {
            current_index = index;
        }
    } else {
        println!("Failed to read directory.");
        app.quit();
    }

    if image_files.is_empty() {
        println!("No images found in the directory. Exiting.");
        app.quit()
    }

    wind.handle(move |mut wind, event| {
        use fltk::enums::Event;
        match event {
            Event::MouseWheel => {
                let dy = app::event_dy();
                let mouse_pos = (app::event_x(), app::event_y());
                let base_zoom_speed = 0.2;
                let mut relative_pos = (0, 0);
                println!("Wind width/height: {}, {}", wind.width(), wind.height());

                if dy == MouseWheel::Up {
                    println!("Zooming out");
                    zoom_factor -= base_zoom_speed * zoom_factor;
                    relative_pos = (-mouse_pos.0 + (wind.width() as f64 / 2.0) as i32, -mouse_pos.1 + (wind.height() as f64 / 2.0) as i32);
                } else if dy == MouseWheel::Down {
                    println!("Zooming in");
                    zoom_factor += base_zoom_speed * zoom_factor;
                    relative_pos = (mouse_pos.0 - (wind.width() as f64 / 2.0) as i32, mouse_pos.1 - (wind.height() as f64 / 2.0) as i32);
                }
                println!("Relative pos: {:?}", relative_pos);
                if zoom_factor < 1.0 {
                    zoom_factor = 1.0; // Don't zoom out beyond the original size
                }
                let new_width = (original_image.width() as f64 * zoom_factor) as i32;
                let new_height = (original_image.height() as f64 * zoom_factor) as i32;
                let new_pos_x = frame.x() - relative_pos.0/2;
                let new_pos_y = frame.y() - relative_pos.1/2;

                println!("Zoom factor: {}", zoom_factor);
                println!("New X/Y: {}, {}", new_pos_x, new_pos_y);
                println!("New width/height: {}, {}", new_width, new_height);

                // Recenter image if we zoomed out all the way
                if zoom_factor > 1.0 {
                    frame.set_pos(new_pos_x, new_pos_y);
                } else {
                    frame.set_pos(0, 0);
                }

                frame.set_image(Some(original_image.copy_sized(new_width, new_height)));
                wind.redraw(); 
                true
            }
            Event::Push => {
                pan_origin = Some((app::event_x(), app::event_y()));
                true
            }
            Event::Drag => {
                if let Some((start_x, start_y)) = pan_origin {
                    let dx = app::event_x() - start_x;
                    let dy = app::event_y() - start_y;
                    frame.set_pos(frame.x() + dx, frame.y() + dy);
                    pan_origin = Some((app::event_x(), app::event_y()));
                    wind.redraw();
                    true
                } else {
                    false
                }
            }
            Event::KeyDown => {
                let key = app::event_key();
                if image_files.is_empty() {                            
                    app.quit();
                }
                match key {
                    fltk::enums::Key::Left => {
                        current_index = (current_index + image_files.len() - 1) % image_files.len();
                        println!("Loading previous image: {}", image_files[current_index].display());
                        load_and_display_image(&mut original_image, &mut frame, &mut wind, &image_files[current_index], &mut zoom_factor);
                    }
                    fltk::enums::Key::Right => {
                        current_index = (current_index + 1) % image_files.len();
                        println!("Loading next image: {}", image_files[current_index].display());
                        load_and_display_image(&mut original_image, &mut frame, &mut wind, &image_files[current_index], &mut zoom_factor);
                    }
                    fltk::enums::Key::Home => {
                        current_index = 0;
                        println!("Loading first image: {}", image_files[current_index].display());
                        load_and_display_image(&mut original_image, &mut frame, &mut wind, &image_files[current_index], &mut zoom_factor);
                    }
                    fltk::enums::Key::End => {
                        current_index = image_files.len() - 1;
                        println!("Loading last image: {}", image_files[current_index].display());
                        load_and_display_image(&mut original_image, &mut frame, &mut wind, &image_files[current_index], &mut zoom_factor);
                    }
                    fltk::enums::Key::Delete => {
                        if dialog::choice2(wind.width()/2 - 200, wind.height()/2 - 100, "Do you want to delete this image file?", "Cancel", "Delete", "") == Some(1) {
                            println!("Delete image: {}", image_files[current_index].display());
                            if let Err(err) = fs::remove_file(&image_files[current_index]) {
                                println!("Failed to delete image: {}", err);
                            } else {
                                image_files.remove(current_index);
                                if image_files.is_empty() {
                                    app.quit();
                                } else {
                                    current_index = current_index % image_files.len();
                                    load_and_display_image(&mut original_image, &mut frame, &mut wind, &image_files[current_index], &mut zoom_factor);
                                }
                            }
                        } else {
                            println!("Delete cancelled");
                        };
                    }
                    fltk::enums::Key::Escape => {
                        app.quit();
                    }
                    _ => (),
                }
                true
            }
            _ => false,
        }
    });

    app.run()?;
    Ok(())
}
