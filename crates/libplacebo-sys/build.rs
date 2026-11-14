//! Builds the bindings, the libav glue, the link directives and the DLL copy
//! for the libplacebo this repository compiles.
//!
//! Nothing here reads pkg-config: libplacebo is one prefix on this machine, and
//! `FFmpeg` and the Vulkan SDK are two more, so each path is a knob with an
//! environment override and a sensible default. That is what makes
//! `cargo run --bin video` work without exporting anything first.
//!
//! `ffcine-libplacebo-sys` does the same job through pkg-config, which needs
//! `PKG_CONFIG_PATH`, `CC` and `INCLUDE` in the environment — and Cargo can only
//! set those for the whole workspace, where they would break the Linux builds of
//! the Twitch crate.

use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

/// `meson install`'s prefix for the libplacebo this repo builds.
const DEFAULT_PLACEBO: &str = "C:/Users/alex/.local/libplacebo-shared";
/// The scoop `FFmpeg` package.
const DEFAULT_FFMPEG: &str = "C:/Users/alex/scoop/apps/ffmpeg-shared/current";
/// The Vulkan SDK, for `vulkan/vulkan.h`.
const DEFAULT_VULKAN: &str = "C:/Users/alex/scoop/apps/vulkan/current";
/// The xwin splat holding the MSVC CRT and Windows SDK.
const DEFAULT_SPLAT: &str = "C:/Users/alex/Projects/Code/Libs/Windows/splat";

fn main() {
    for knob in [
        "LIBPLACEBO_PREFIX",
        "FFMPEG_DIR",
        "VULKAN_SDK",
        "LIBPLACEBO_SPLAT",
        "LIBCLANG_PATH",
    ] {
        println!("cargo:rerun-if-env-changed={knob}");
    }
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=src/libav.c");

    let placebo = prefix("LIBPLACEBO_PREFIX", DEFAULT_PLACEBO);
    let ffmpeg = prefix("FFMPEG_DIR", DEFAULT_FFMPEG);
    let vulkan = prefix("VULKAN_SDK", DEFAULT_VULKAN);
    let splat = prefix("LIBPLACEBO_SPLAT", DEFAULT_SPLAT);

    let includes = Includes {
        placebo: placebo.join("include"),
        ffmpeg: ffmpeg.join("include"),
        vulkan: vulkan.join("Include"),
        crt: splat.join("VC/Tools/MSVC").join(latest(&splat.join("VC/Tools/MSVC"))).join("include"),
        ucrt: sdk_include(&splat, "ucrt"),
        um: sdk_include(&splat, "um"),
        shared: sdk_include(&splat, "shared"),
    };

    bindings(&includes);
    glue(&includes);
    link(&placebo);
}

/// Every include directory the bindings and the glue need, in one place.
struct Includes {
    placebo: PathBuf,
    ffmpeg: PathBuf,
    vulkan: PathBuf,
    crt: PathBuf,
    ucrt: PathBuf,
    um: PathBuf,
    shared: PathBuf,
}

impl Includes {
    /// The two the public headers need: libplacebo itself and Vulkan.
    fn public(&self) -> [&Path; 2] {
        [&self.placebo, &self.vulkan]
    }

    /// Everything `cc` needs to compile C for this ABI.
    fn all(&self) -> [&Path; 7] {
        [
            &self.placebo,
            &self.ffmpeg,
            &self.vulkan,
            &self.crt,
            &self.ucrt,
            &self.um,
            &self.shared,
        ]
    }
}

/// Generates the bindings from the installed headers.
fn bindings(includes: &Includes) {
    for dir in includes.public() {
        rerun_on_dir(dir);
    }
    let mut builder = bindgen::Builder::default()
        .header("wrapper.h")
        .allowlist_function("pl_.*")
        .allowlist_type("pl_.*")
        .allowlist_var("pl_.*|PL_.*")
        .default_enum_style(bindgen::EnumVariation::Rust {
            non_exhaustive: true,
        })
        .derive_default(true)
        .layout_tests(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    for dir in includes.all() {
        builder = builder.clang_arg(format!("-I{}", dir.display()));
    }
    builder
        .generate()
        .expect("bindgen could not read libplacebo's headers")
        .write_to_file(out_dir().join("bindings.rs"))
        .expect("could not write bindings.rs");
}

/// Compiles `libav.c`, which is the only part of libplacebo's libav support that
/// is not `static inline`.
///
/// The compiler is named rather than taken from `CC`: `cc` would otherwise look
/// for `cl.exe`, which this machine does not have.
fn glue(includes: &Includes) {
    let mut build = cc::Build::new();
    build.compiler("clang-cl").file("src/libav.c").warnings(false);
    for dir in includes.all() {
        build.include(dir);
    }
    build.compile("placebo_libav");
}

/// Points the linker at the import library and puts the DLL beside the binary.
fn link(placebo: &Path) {
    let lib = placebo.join("lib");
    println!("cargo:rustc-link-search=native={}", lib.display());
    // `libplacebo.lib`, from the shared build.
    println!("cargo:rustc-link-lib=libplacebo");
    copy_dll(placebo);
}

/// Copies `libplacebo-<api>.dll` into the profile directory.
///
/// Windows resolves it when the process starts, before `main`, and `cargo run`
/// gives the process the environment it already had, so the executable's own
/// directory is the only place the loader will look.
fn copy_dll(placebo: &Path) {
    let Some(profile) = profile_dir() else {
        println!("cargo:warning=cannot work out the profile directory from OUT_DIR");
        return;
    };
    let Ok(entries) = fs::read_dir(placebo.join("bin")) else {
        return;
    };
    let mut found = false;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("libplacebo") && name.ends_with(".dll")) {
            continue;
        }
        found = true;
        println!("cargo:rerun-if-changed={}", entry.path().display());
        let destination = profile.join(name.as_ref());
        let fresh = match (entry.metadata(), fs::metadata(&destination)) {
            (Ok(source), Ok(copied)) => match (source.modified(), copied.modified()) {
                (Ok(source), Ok(copied)) => copied >= source,
                _ => false,
            },
            _ => false,
        };
        if fresh {
            continue;
        }
        if let Err(error) = fs::copy(entry.path(), &destination) {
            println!(
                "cargo:warning=could not copy {} to {}: {error}",
                entry.path().display(),
                profile.display()
            );
        }
    }
    if !found {
        println!(
            "cargo:warning=no libplacebo DLL in {}; a static build needs none",
            placebo.join("bin").display()
        );
    }
}

/// `target/<profile>`, the parent of the `build` directory in `OUT_DIR`.
///
/// Cargo has used both `build/<crate>-<hash>/out` and `build/<crate>/<hash>/out`,
/// so this walks to `build` instead of counting levels.
fn profile_dir() -> Option<PathBuf> {
    let out = out_dir();
    let build = out
        .ancestors()
        .find(|path| path.file_name().is_some_and(|name| name == "build"))?;
    build.parent().map(Path::to_path_buf)
}

fn out_dir() -> PathBuf {
    PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set for build scripts"))
}

/// A path from `knob`, or `default` when it is not set.
fn prefix(knob: &str, default: &str) -> PathBuf {
    let value = env::var_os(knob).map_or_else(|| PathBuf::from(default), PathBuf::from);
    assert!(
        value.is_dir(),
        "{knob} points at {}, which is not a directory",
        value.display()
    );
    value
}

/// `Windows Kits/10/Include/<version>/<part>`.
fn sdk_include(splat: &Path, part: &str) -> PathBuf {
    let include = splat.join("Windows Kits/10/Include");
    include.join(latest(&include)).join(part)
}

/// The newest subdirectory of `dir`, which is how the splat versions itself.
fn latest(dir: &Path) -> String {
    let versions: Vec<String> = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("cannot list {}: {error}", dir.display()))
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    versions
        .into_iter()
        .max()
        .unwrap_or_else(|| panic!("no versions under {}", dir.display()))
}

/// Rebuilds the bindings when a header changes.
fn rerun_on_dir(dir: &Path) {
    println!("cargo:rerun-if-changed={}", dir.display());
}
