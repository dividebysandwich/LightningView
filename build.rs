#[cfg(target_os = "windows")]
extern crate winres;

use std::path::Path;
use std::process::Command;

fn main() {
    compile_shaders();

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

/// Compile the GLSL shaders under `shaders/` to SPIR-V bytecode (for the Vulkan
/// SDL_GPU backend) using `glslc`. The compiled `.spv` blobs land in `OUT_DIR`
/// and are pulled into the binary with `include_bytes!` from `gpu.rs`.
///
/// Note: this currently only emits SPIR-V. Windows (DXIL) and macOS (MSL)
/// backends will additionally need their bytecode produced via SDL_shadercross;
/// the renderer already selects the blob matching the device's shader formats.
fn compile_shaders() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    // Each entry: (source file, glslc stage flag).
    let shaders = [
        ("shaders/quad.vert", "vertex"),
        ("shaders/quad.frag", "fragment"),
        ("shaders/video.frag", "fragment"),
    ];

    for (src, stage) in shaders {
        println!("cargo:rerun-if-changed={src}");
        let file_name = Path::new(src).file_name().unwrap().to_str().unwrap();
        let out_path = format!("{out_dir}/{file_name}.spv");

        let status = Command::new("glslc")
            .arg(format!("-fshader-stage={stage}"))
            .arg(src)
            .arg("-o")
            .arg(&out_path)
            .status();

        match status {
            Ok(s) if s.success() => {}
            Ok(s) => panic!("glslc failed to compile {src} (exit {s})"),
            Err(e) => panic!(
                "failed to run glslc for {src}: {e}. \
                 Install shaderc (glslc) to build the SDL_GPU shaders."
            ),
        }
    }
}
