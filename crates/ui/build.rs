//! Compiles the presentation shaders with `glslangValidator`.
//!
//! The shaders are GLSL because the device is Vulkan and the pass is thirty
//! lines; going through a WGSL front end to reach SPIR-V would only put an
//! abstraction between the shader and the bug being chased.
//!
//! Set `GLSLANG` to point at a validator that is not on `PATH`.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=shaders");
    println!("cargo:rerun-if-env-changed=GLSLANG");

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let glslang = std::env::var("GLSLANG").unwrap_or_else(|_| String::from("glslangValidator"));

    for shader in ["ui.vert", "ui.frag", "scan.comp"] {
        let source = manifest.join("shaders").join(shader);
        let compiled = out.join(format!("{shader}.spv"));
        println!("cargo:rerun-if-changed=shaders/{shader}");

        let status = Command::new(&glslang)
            .args(["-V", "--target-env", "vulkan1.2"])
            .arg("-o")
            .arg(&compiled)
            .arg(&source)
            .status()
            .unwrap_or_else(|error| {
                panic!("could not run {glslang} for {shader}: {error}; set GLSLANG to its path")
            });
        assert!(status.success(), "{glslang} rejected {shader}");
    }
}
