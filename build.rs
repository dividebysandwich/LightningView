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

/// Provide each shader's SPIR-V to `OUT_DIR` (where `gpu.rs` `include_bytes!`s it).
///
/// SPIR-V is platform-independent, so we check the compiled `.spv` blobs into the
/// repo (`shaders/*.spv`) and use those directly — no shader toolchain is needed
/// to build on any platform/CI. When `glslc` *is* available we recompile from the
/// `.glsl` source (so local edits take effect immediately) and warn if the
/// checked-in blob is out of date and should be regenerated + committed.
///
/// Regenerate the checked-in blobs after editing a shader with `shaders/compile.sh`.
fn compile_shaders() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    // Each entry: (source file, glslc stage flag).
    let shaders = [
        ("shaders/quad.vert", "vertex"),
        ("shaders/quad.frag", "fragment"),
        ("shaders/video.frag", "fragment"),
    ];

    // `LV_FORCE_PREBUILT_SHADERS=1` forces the checked-in-SPIR-V path (what CI
    // uses), so it can be exercised locally even when glslc is installed.
    println!("cargo:rerun-if-env-changed=LV_FORCE_PREBUILT_SHADERS");
    let force_prebuilt = std::env::var_os("LV_FORCE_PREBUILT_SHADERS").is_some();
    let have_glslc =
        !force_prebuilt && Command::new("glslc").arg("--version").output().is_ok();

    for (src, stage) in shaders {
        let file_name = Path::new(src).file_name().unwrap().to_str().unwrap();
        let prebuilt = format!("{manifest_dir}/shaders/{file_name}.spv");
        let out_path = format!("{out_dir}/{file_name}.spv");
        println!("cargo:rerun-if-changed={src}");
        println!("cargo:rerun-if-changed=shaders/{file_name}.spv");

        if have_glslc {
            // Compile fresh from source so local shader edits are picked up.
            let status = Command::new("glslc")
                .arg(format!("-fshader-stage={stage}"))
                .arg(src)
                .arg("-o")
                .arg(&out_path)
                .status();
            match status {
                Ok(s) if s.success() => {}
                Ok(s) => panic!("glslc failed to compile {src} (exit {s})"),
                Err(e) => panic!("failed to run glslc for {src}: {e}"),
            }
            // Nudge the developer to refresh the committed blob if it drifted.
            if std::fs::read(&out_path).ok() != std::fs::read(&prebuilt).ok() {
                println!(
                    "cargo:warning=Checked-in {file_name}.spv differs from a fresh \
                     glslc build — run shaders/compile.sh and commit the result."
                );
            }
        } else {
            // No shader compiler (typical on CI): use the checked-in SPIR-V.
            std::fs::copy(&prebuilt, &out_path).unwrap_or_else(|e| {
                panic!(
                    "no shader compiler (glslc) and no prebuilt {prebuilt}: {e}. \
                     Commit shaders/{file_name}.spv (see shaders/compile.sh)."
                )
            });
        }
    }
}
