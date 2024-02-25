use fltk::{app::{self, MouseWheel}, enums::Color, frame::Frame, image::SharedImage, prelude::*, window::Window};
use std::{env, error::Error, fs, path::{Path, PathBuf}};

fn load_and_display_image(original_image: &mut SharedImage, frame: &mut Frame, path: &PathBuf) {
    if let Ok(image) = SharedImage::load(&path) {
        let mut new_image = image.clone();
        new_image.scale(frame.width(), frame.height(), true, true);
        frame.set_image(Some(new_image));
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

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        println!("Usage: {} <image_file>", args[0]);
        std::process::exit(1);
    }

    let image_file = &args[1];

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

    let mut original_image = SharedImage::load(image_file)?;
    let mut image = original_image.clone();
    image.scale(wind.width(), wind.height(), true, true);

    frame.set_image(Some(image));
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
        image_files = entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| {
                path.is_file()
                    && path != Path::new(image_file)
                    && (path.extension().map_or(false, |ext| ext == "jpg")
                        || path.extension().map_or(false, |ext| ext == "jpeg")
                        || path.extension().map_or(false, |ext| ext == "png")
                        || path.extension().map_or(false, |ext| ext == "bmp")
                        || path.extension().map_or(false, |ext| ext == "gif")
                    )
            })
            .collect();
        image_files.sort();
    } else {
        println!("Failed to read directory.");
    }

    wind.handle(move |wind, event| {
        use fltk::enums::Event;
        match event {
            Event::MouseWheel => {
                let dy = app::event_dy();
                let mouse_pos = (app::event_x(), app::event_y());
                let base_zoom_speed = 0.2; // Adjust zoom speed as needed
                let mut relative_pos = (0, 0);

                if dy == MouseWheel::Up {
                    println!("Zooming out");
                    zoom_factor -= base_zoom_speed * zoom_factor;
                    relative_pos = (-mouse_pos.0 + wind.width() / 2, mouse_pos.1 - wind.height() / 2);
                } else if dy == MouseWheel::Down {
                    println!("Zooming in");
                    zoom_factor += base_zoom_speed * zoom_factor;
                    relative_pos = (mouse_pos.0 - wind.width() / 2, -mouse_pos.1 + wind.height() / 2);
                }
                if zoom_factor < 0.1 {
                    zoom_factor = 0.1; // Minimum zoom factor
                }
                let mut image = original_image.clone();
                let new_width = (wind.width() as f64 * zoom_factor) as i32;
                let new_height = (wind.height() as f64 * zoom_factor) as i32;
                frame.set_pos(frame.x() - relative_pos.0/2, frame.y() + relative_pos.1/2);
                image.scale(new_width, new_height, true, true);
                frame.set_image(Some(image));
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
                match key {
                    fltk::enums::Key::Left => {
                        if !image_files.is_empty() {                            
                            current_index = (current_index + image_files.len() - 1) % image_files.len();
                            println!("Loading previous image: {}", image_files[current_index].display());
                            load_and_display_image(&mut original_image, &mut frame, &image_files[current_index]);
                            zoom_factor = 1.0;
                            frame.set_pos(0, 0);
                            wind.redraw();
                        }
                    }
                    fltk::enums::Key::Right => {
                        if !image_files.is_empty() {
                            current_index = (current_index + 1) % image_files.len();
                            println!("Loading next image: {}", image_files[current_index].display());
                            load_and_display_image(&mut original_image, &mut frame, &image_files[current_index]);
                            zoom_factor = 1.0;
                            frame.set_pos(0, 0);
                            wind.redraw();
                        }
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
