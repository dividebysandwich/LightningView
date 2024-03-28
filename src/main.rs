#![cfg_attr(
    all(
      target_os = "windows",
    ),
    windows_subsystem = "windows"
  )]
use fltk::{app::{self, MouseWheel}, dialog, enums::{Color, Event}, frame::Frame, image::{AnimGifImage, AnimGifImageFlags, SharedImage}, prelude::*, window::Window};
use arboard::{Clipboard, ImageData};
use rand::seq::SliceRandom;
use std::{env, error::Error, fs, path::{Path, PathBuf}};
use image::io::Reader as ImageReader;
use image::GenericImageView;
use log;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use crate::windows::*;

pub const IMAGEREADER_SUPPORTED_FORMATS: [&str; 4] = ["webp", "tif", "tiff", "tga"];
pub const ANIM_SUPPORTED_FORMATS: [&str; 1] = ["gif"];
pub const FLTK_SUPPORTED_FORMATS: [&str; 9] = ["jpg", "jpeg", "png", "bmp", "svg", "ico", "pnm", "xbm", "xpm"];
pub const RAW_SUPPORTED_FORMATS: [&str; 23] = ["mrw", "arw", "srf", "sr2", "nef", "mef", "orf", "srw", "erf", "kdc", "dcs", "rw2", "raf", "dcr", "dng", "pef", "crw", "iiq", "3fr", "nrw", "mos", "cr2", "ari"];

const KEY_C : fltk::enums::Key = fltk::enums::Key::from_char('c');

// Enum to hold the image type, either a shared image or an animated gif
#[derive(Clone)]
enum ImageType {
    Shared(SharedImage),
    AnimatedGif(AnimGifImage),
}

fn load_and_display_image(original_image: &mut ImageType, frame: &mut Frame, wind: &mut Window, path: &PathBuf, zoom_factor: &mut f64, is_fullscreen: bool) {
    if let Ok(image) = load_image(&path.to_string_lossy(), wind) {
        frame.set_pos(0, 0);
        let cloned_image = image.clone();
        match cloned_image {
            ImageType::Shared(img) => {
                let mut new_image = img.clone();
                new_image.scale(wind.width(), wind.height(), true, true);
                frame.set_image(Some(new_image));
            },
            ImageType::AnimatedGif(mut anim_img) => {
                anim_img.scale(wind.width(), wind.height(), true, true);
                frame.set_image(Some(anim_img.clone()));
            }
        }
        wind.redraw();
        wind.fullscreen(is_fullscreen);

        *zoom_factor = 1.0;
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

fn load_imagereader(image_file: &str) -> Result<SharedImage, String> {
    log::debug!("Processing with Imagereader: {}", image_file);

    let reader = ImageReader::open(image_file)
        .map_err(|err| format!("Don't know how to load \"{}\": {}", image_file, err))?;

    let decoded_image = reader
        .decode()
        .map_err(|err| format!("Decoding \"{}\" failed: {}", image_file, err))?;

    let (width, height) = decoded_image.dimensions();
    log::debug!("Image dimensions: {}x{}", width, height);
    log::debug!("Image color type: {:?}", decoded_image.color());

    let data = decoded_image.into_rgb8().to_vec();
    let img = fltk::image::RgbImage::new(
        &data,
        width as i32,
        height as i32,
        fltk::enums::ColorDepth::Rgb8,
    )
    .map_err(|err| format!("Processing \"{}\" failed: {}", image_file, err))?;

    SharedImage::from_image(img).map_err(|err| format!("Error creating image: {}", err))
}

fn load_raw(image_file: &str) -> Result<SharedImage, String> {
    log::debug!("Processing as RAW: {}", image_file);

    let mut pipeline = imagepipe::Pipeline::new_from_file(image_file)
        .map_err(|err| format!("Don't know how to load \"{}\": {}", image_file, err))?;

    let decoded = pipeline
        .output_8bit(Some(&imagepipe::Pipeline::new_cache(100_000_000)))
        .map_err(|err| format!("Processing for \"{}\" failed: {}", image_file, err))?;

    let img = fltk::image::RgbImage::new(
        &decoded.data,
        decoded.width as i32,
        decoded.height as i32,
        fltk::enums::ColorDepth::Rgb8,
    )
    .map_err(|err| format!("Processing for \"{}\" failed: {}", image_file, err))?;

    SharedImage::from_image(img).map_err(|err| format!("Error creating image: {}", err))
}

fn load_animated_image(image_file: &str, widget: &mut Window) -> Result<AnimGifImage, String> {
    log::debug!("Processing as animated image: {}", image_file);
    let anim_image = AnimGifImage::load(image_file, widget, AnimGifImageFlags::DONT_RESIZE_CANVAS)
        .map_err(|err| format!("Error loading animated image: {}", err))?;

    Ok(anim_image)
}

fn load_image(image_file: &str, widget: &mut Window) -> Result<ImageType, String> {
    if FLTK_SUPPORTED_FORMATS.iter().any(|&format| image_file.to_lowercase().ends_with(format)) {
        match SharedImage::load(image_file) {
            Ok(image) => Ok(ImageType::Shared(image)),
            Err(err) => Err(format!("Error loading image: {}", err)),
        }
    } else if ANIM_SUPPORTED_FORMATS.iter().any(|&format| image_file.to_lowercase().ends_with(format)) {
        match load_animated_image(image_file, widget) {
            Ok(image) => {
                Ok(ImageType::AnimatedGif(image))
            },
            Err(err) => Err(format!("Error loading animated GIF image: {}", err)),
        }
    } else if RAW_SUPPORTED_FORMATS.iter().any(|&format| image_file.to_lowercase().ends_with(format)) {
        match load_raw(image_file) {
            Ok(image) => Ok(ImageType::Shared(image)),
            Err(err) => Err(format!("Error loading RAW image: {}", err)),
        }
    } else if IMAGEREADER_SUPPORTED_FORMATS.iter().any(|&format| image_file.to_lowercase().ends_with(format)) {
        match load_imagereader(image_file) {
            Ok(image) => Ok(ImageType::Shared(image)),
            Err(err) => Err(format!("Error loading Imagereader image: {}", err)),
        }
    } else {
        Err("Unsupported file format.".to_string())
    }
}

fn copy_to_clipboard(original_image: &mut ImageType, clipboard: &mut Clipboard) -> Result<(), String> {
    match &original_image {
        ImageType::Shared(img) => {
            match img.depth() {
                fltk::enums::ColorDepth::Rgba8 => {
                    let rgba_image = img.to_rgb()
                        .map_err(|err| format!("Error converting SharedImage to RGB: {}", err))?;
                    let rgb_data = rgba_image.to_rgb_data();
                    let img_data: ImageData = ImageData {
                        bytes: rgb_data.into(),
                        width: img.data_w() as usize,
                        height: img.data_h() as usize,
                    };
                    let _ = clipboard.set_image(img_data);
                    log::debug!("Image copied to clipboard");
                    Ok(())
                },
                fltk::enums::ColorDepth::Rgb8 => {
                    let rgb_image = img.to_rgb()
                        .map_err(|err| format!("Error converting SharedImage to RGB: {}", err))?;
                    let rgba_image = rgb_image.convert(fltk::enums::ColorDepth::Rgba8)
                        .map_err(|err| format!("Error converting RGB to RGBA: {}", err))?;
                    let rgba_data = rgba_image.to_rgb_data();
                    log::debug!("rgba image size: {}", rgba_data.len());
                    let img_data: ImageData = ImageData {
                        bytes: rgba_data.into(),
                        width: img.data_w() as usize,
                                height: img.data_h() as usize,
                    };
                    let _ = clipboard.set_image(img_data);
                    log::debug!("Image copied to clipboard");
                    Ok(())
                },
                _ => {
                    Err(format!("Unsupported color depth: {:?}", img.depth()))
                }
            }
        },
        ImageType::AnimatedGif(_anim_img) => {
            Err(format!("Copying animated images to clipboard is not supported"))
        }
    }
}


fn main() -> Result<(), Box<dyn Error>> {
//    std::env::set_var("RUST_LOG", "debug");
    env_logger::init();

    let args: Vec<String> = env::args().collect();
    let mut is_fullscreen = true;
    let mut image_order:Vec<usize> = Vec::new();
    let mut clipboard: Clipboard = Clipboard::new().unwrap();

    if args.len() < 2 {
        println!("Usage: {} <image_file>", args[0]);
        #[cfg(target_os = "windows")]
        {
            println!("To register as image viewer in Windows, run: {} /register", args[0]);
            println!("To unregister, run: {} /unregister", args[0]);
        }
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

    // Create an empty mutable image to be able to modify it later
    let empty_img = fltk::image::RgbImage::new(&[0; 4], 1, 1, fltk::enums::ColorDepth::Rgb8).unwrap();
    let mut original_image = ImageType::Shared(SharedImage::from_image(empty_img).unwrap());

    let app = app::App::default();

    // Enable bilinear filtering for scaling operations
    fltk::image::RgbImage::set_scaling_algorithm(fltk::image::RgbScaling::Bilinear);

    let mut zoom_factor = 1.0;
    let mut pan_origin: Option<(i32, i32)> = None;
    let mut current_index = 0;
    let mut image_files: Vec<PathBuf> = Vec::new();
    
    // Get the screen size
    let screen = app::screen_count(); // Get the number of screens
    let (screen_width, screen_height) = if screen > 0 {
        let screen = app::screen_xywh(0); // Get the work area of the primary screen
        (screen.2, screen.3)
    } else {
        (800, 600) // Default dimensions
    };

    log::debug!("Image file: {}", image_file);

    let absolute_path = get_absolute_path(image_file);
    let parent_dir = absolute_path.parent().unwrap_or_else(|| {
        println!("Failed to get the parent directory.");
        std::process::exit(1);
    });

    log::debug!("Parent dir: {:?}", parent_dir);

    // Get a list of all image files in the directory
    if let Ok(entries) = fs::read_dir(parent_dir) {
        let mut all_supported_formats: Vec<&str> = Vec::new();
        all_supported_formats.extend(&IMAGEREADER_SUPPORTED_FORMATS);
        all_supported_formats.extend(&ANIM_SUPPORTED_FORMATS);
        all_supported_formats.extend(&FLTK_SUPPORTED_FORMATS);
        all_supported_formats.extend(&RAW_SUPPORTED_FORMATS);
        image_files = entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| {
                path.is_file()
                    && all_supported_formats.iter().any(|&format| path.to_string_lossy().to_lowercase().ends_with(format) 
                )
            })
            .collect();

        //Sort files by name, case insensitive
        image_files.sort_by_key(|name| name.to_string_lossy().to_lowercase());
        
        // Find out where in the list our initially loaded file is, so we can navigate to the next/previous image
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

    // Initialize the image_order list with a sequential index so they are browsed in-sequence
    for (i, _path) in image_files.iter().enumerate() {
        image_order.push(i);
    }

    let mut wind = Window::new(0, 0, screen_width, screen_height, "Lightning View");
    wind.make_resizable(true);
    wind.set_color(Color::Black);
    wind.fullscreen(is_fullscreen);
    let mut frame = Frame::default_fill();
    wind.end(); // Finish adding UI components to the window

    // Load and display the initial image
    load_and_display_image(&mut original_image, &mut frame, &mut wind, &image_files[image_order[current_index]], &mut zoom_factor, is_fullscreen);

    wind.show();


    wind.handle(move |mut wind, event| {
        match event {
            Event::Focus => true,
            Event::Leave => true,
            Event::MouseWheel => {
                let dy = app::event_dy();
                let mouse_pos = (app::event_x(), app::event_y());
                let base_zoom_speed = 0.2;
                let mut relative_pos = (0, 0);
                log::debug!("Wind width/height: {}, {}", wind.width(), wind.height());

                if dy == MouseWheel::Up {
                    log::debug!("Zooming out");
                    zoom_factor -= base_zoom_speed * zoom_factor;
                    relative_pos = (-mouse_pos.0 + (wind.width() as f64 / 2.0) as i32, -mouse_pos.1 + (wind.height() as f64 / 2.0) as i32);
                } else if dy == MouseWheel::Down {
                    log::debug!("Zooming in");
                    zoom_factor += base_zoom_speed * zoom_factor;
                    relative_pos = (mouse_pos.0 - (wind.width() as f64 / 2.0) as i32, mouse_pos.1 - (wind.height() as f64 / 2.0) as i32);
                }
                log::debug!("Relative pos: {:?}", relative_pos);
                if zoom_factor < 1.0 {
                    zoom_factor = 1.0; // Don't zoom out beyond the original size
                }

                match &original_image {
                    ImageType::Shared(img) => {
                        let new_image = img.clone();
                        let new_width = (new_image.width() as f64 * zoom_factor) as i32;
                        let new_height = (new_image.height() as f64 * zoom_factor) as i32;
                        log::debug!("New width/height: {}, {}", new_width, new_height);
                        frame.set_image(Some(new_image.copy_sized(new_width, new_height)));
                    },
                    ImageType::AnimatedGif(anim_img) => {
                        let new_image = anim_img.clone();
                        let new_width = (new_image.width() as f64 * zoom_factor) as i32;
                        let new_height = (new_image.height() as f64 * zoom_factor) as i32;
                        log::debug!("New width/height: {}, {}", new_width, new_height);
                        frame.set_image(Some(new_image.copy_sized(new_width, new_height)));
                    }
                
                }

                let new_pos_x = frame.x() - relative_pos.0/2;
                let new_pos_y = frame.y() - relative_pos.1/2;

                // Recenter image if we zoomed out all the way
                if zoom_factor > 1.0 {
                    frame.set_pos(new_pos_x, new_pos_y);
                } else {
                    frame.set_pos(0, 0);
                }

                log::debug!("Zoom factor: {}", zoom_factor);
                log::debug!("New X/Y: {}, {}", new_pos_x, new_pos_y);

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
                        log::debug!("Loading previous image: {}", image_files[image_order[current_index]].display());
                        load_and_display_image(&mut original_image, &mut frame, &mut wind, &image_files[image_order[current_index]], &mut zoom_factor, is_fullscreen);
                    }
                    fltk::enums::Key::Right => {
                        current_index = (current_index + 1) % image_files.len();
                        log::debug!("Loading next image: {}", image_files[image_order[current_index]].display());
                        load_and_display_image(&mut original_image, &mut frame, &mut wind, &image_files[image_order[current_index]], &mut zoom_factor, is_fullscreen);
                    }
                    fltk::enums::Key::Home => {
                        current_index = 0;
                        log::debug!("Loading first image: {}", image_files[image_order[current_index]].display());
                        load_and_display_image(&mut original_image, &mut frame, &mut wind, &image_files[image_order[current_index]], &mut zoom_factor, is_fullscreen);
                    }
                    fltk::enums::Key::End => {
                        current_index = image_files.len() - 1;
                        log::debug!("Loading last image: {}", image_files[image_order[current_index]].display());
                        load_and_display_image(&mut original_image, &mut frame, &mut wind, &image_files[image_order[current_index]], &mut zoom_factor, is_fullscreen);
                    }
                    fltk::enums::Key::Delete => {
                        if dialog::choice2(wind.width()/2 - 200, wind.height()/2 - 100, format!("Do you want to delete {}?", image_files[image_order[current_index]].display()).as_str(), "Cancel", "Delete", "") == Some(1) {
                            log::debug!("Delete image: {}", image_files[image_order[current_index]].display());
                            if let Err(err) = fs::remove_file(&image_files[image_order[current_index]]) {
                                println!("Failed to delete image: {}", err);
                            } else {
                                image_files.remove(image_order[current_index]);
                                if image_files.is_empty() {
                                    app.quit();
                                } else {
                                    current_index = current_index % image_files.len();
                                    load_and_display_image(&mut original_image, &mut frame, &mut wind, &image_files[image_order[current_index]], &mut zoom_factor, is_fullscreen);
                                }
                            }
                        } else {
                            log::debug!("Delete cancelled");
                        };
                    }
                    fltk::enums::Key::Escape => {
                        app.quit();
                    }
                    KEY_C => {
                        let eventstate = app::event_state();
                        //Check if the Control key was held down when the 'C' key was pressed
                        if eventstate.contains(fltk::enums::Shortcut::Ctrl) {
                            log::debug!("Copy image to clipboard");
                            match copy_to_clipboard(&mut original_image, &mut clipboard) {
                                Ok(_) => {
                                    log::debug!("Image copied to clipboard");
                                },
                                Err(err) => {
                                    log::error!("Failed to copy image to clipboard: {}", err);
                                }
                            
                            }
                        }
                        return true;
                    }
                    _ => {
                        if let Some(ch) = app::event_text().chars().next() {
                            if ch.eq_ignore_ascii_case(&'F') {
                                //Toggle fullscreen
                                wind.make_resizable(true);
                                is_fullscreen = !is_fullscreen;
                                wind.fullscreen(is_fullscreen);
                            }
                            if ch.eq_ignore_ascii_case(&'R') { //Randomize the sequence of images in the directory when viewing the next/prev image
                                let original_index = image_order[current_index]; //Remember the index of the image we're currently viewing
                                let mut rng = rand::thread_rng();
                                image_order.shuffle(&mut rng);
                                log::debug!("Image ordering randomized");
                                current_index = image_order.iter().position(|&index| index == original_index).unwrap(); //Find the new index of the image we were viewing
                            }
                            if ch.eq_ignore_ascii_case(&'N') { // Sort images by name when viewing the next/prev image
                                let original_index = image_order[current_index]; // Remember the index of the image we're currently viewing
                                image_order.sort(); // Sort the image_order list to the original sequence
                                log::debug!("Image ordering sorted by name");
                                current_index = image_order.iter().position(|&index| index == original_index).unwrap(); //Find the new index of the image we were viewing
                            }
                        }
                    }
                }
                true
            }
            _ => false,
        }
    });

    app.run()?;
    Ok(())
}
