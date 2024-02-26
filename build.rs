extern crate winres;

fn main() {
  if cfg!(target_os = "windows") {
    let mut res = winres::WindowsResource::new();
    res.set_icon("lightningview.ico"); // Replace this with the filename of your .ico file.
    res.compile().unwrap();
  }
}
