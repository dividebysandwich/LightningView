#[cfg(target_os = "windows")]
extern crate winres;

use std::path::Path;
use std::process::Command;

fn main() {
    compile_shaders();
    link_macos_clang_rt();

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

/// On macOS, link clang's compiler runtime so SDL3's Objective-C `@available`
/// checks resolve. clang lowers `@available` to a call to `__isPlatformVersionAtLeast`,
/// which lives in `libclang_rt.osx.a` (compiler-rt). rustc drives the final link
/// with `-nodefaultlibs`, which omits that runtime, so without this the SDL3
/// objects come up with `Undefined symbols: ___isPlatformVersionAtLeast`.
fn link_macos_clang_rt() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let cc = std::env::var("CC").unwrap_or_else(|_| "clang".to_string());
    let resource_dir = Command::new(&cc)
        .arg("-print-resource-dir")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|d| !d.is_empty());

    match resource_dir {
        Some(dir) => {
            // The macOS builtins archive lives at <resource-dir>/lib/darwin/libclang_rt.osx.a.
            println!("cargo:rustc-link-search=native={dir}/lib/darwin");
            println!("cargo:rustc-link-lib=static=clang_rt.osx");
        }
        None => println!(
            "cargo:warning=could not determine clang resource dir; SDL3 @available \
             symbols (__isPlatformVersionAtLeast) may fail to link"
        ),
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

    // SDL_GPU consumes a different bytecode per backend. D3D12 (Windows) needs
    // DXIL and Metal (macOS) needs MSL, so on those targets transpiling from the
    // SPIR-V via SDL_shadercross is mandatory — without it the app could only run
    // under Vulkan. Elsewhere (Linux/Vulkan) the native formats are optional.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let dxil_required = target_os == "windows";
    let msl_required = target_os == "macos";

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

        // Produce the per-backend native formats next to the SPIR-V. `gpu.rs`
        // `include_bytes!`s all three, so an absent format must still exist as an
        // empty placeholder (the renderer then just won't advertise it).
        transpile_shader(&out_dir, file_name, stage, "dxil", "DXIL", dxil_required);
        transpile_shader(&out_dir, file_name, stage, "msl", "MSL", msl_required);
    }
}

/// Transpile `<out_dir>/<name>.spv` to `<out_dir>/<name>.<ext>` (DXIL/MSL) via the
/// SDL_shadercross CLI. If shadercross is unavailable or fails, panic when the
/// format is `required` for this target, otherwise write an empty placeholder.
fn transpile_shader(out_dir: &str, name: &str, stage: &str, ext: &str, dest: &str, required: bool) {
    let spv = format!("{out_dir}/{name}.spv");
    let out = format!("{out_dir}/{name}.{ext}");

    let status = Command::new("shadercross")
        .args([&spv, "-s", "SPIRV", "-d", dest, "-t", stage, "-o", &out])
        .status();

    let ok = matches!(&status, Ok(s) if s.success());
    if ok {
        return;
    }
    if required {
        panic!(
            "SDL_shadercross is required to build the {dest} shader for {name} on this \
             target, but it isn't available (or failed): {status:?}. Install SDL_shadercross \
             and ensure `shadercross` is on PATH."
        );
    }
    // Optional on this target (e.g. Linux/Vulkan): leave an empty placeholder.
    std::fs::write(&out, []).expect("write empty shader placeholder");
}
