#[cfg(target_os = "windows")]
extern crate winres;

fn main() {
    #[cfg(target_os = "windows")]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("lightningview.ico"); // Replace this with the filename of your .ico file.
        // Friendly, properly-cased name shown by Windows in the "Open with" picker
        // and program lists (otherwise it falls back to the lowercase crate name).
        res.set("ProductName", "LightningView");
        res.set("FileDescription", "LightningView");
        res.compile().unwrap();
    }
}
