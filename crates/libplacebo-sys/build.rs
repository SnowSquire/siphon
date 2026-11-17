//! Builds the bindings, the libav glue, the `pl_log_create` shim, the link
//! directives and the DLL copy for the libplacebo this repository links.
//!
//! Two platforms, two ways of finding the library:
//!
//! * **Windows** uses explicit prefixes (libplacebo, FFmpeg, the Vulkan SDK and
//!   the xwin splat), because the scoop packages ship no pkg-config files and
//!   `clang-cl` needs the MSVC CRT and Windows SDK on its include path. Every
//!   path is a knob with an environment override and a default, which is what
//!   makes `cargo run --bin video` work without exporting anything first.
//! * **Linux** uses pkg-config, because there the library is a distribution
//!   package (or a prefix from `scripts/build-libplacebo.sh`). `LIBPLACEBO_PREFIX`
//!   still wins when it is set.
//!
//! `ffcine-libplacebo-sys` does the same job through pkg-config alone, which needs
//! `PKG_CONFIG_PATH`, `CC` and `INCLUDE` in the environment for every platform —
//! and Cargo can only set those for the whole workspace, where they would break
//! the Twitch crate's Linux builds.

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
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=src/shim.c");
    println!("cargo:rerun-if-changed=src/libav.c");
    // Build scripts run on the host but serve the *target*: `cfg(windows)` here
    // would describe the host, so cross-compiling needs Cargo's target variable.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows();
    } else {
        linux();
    }
}

// ---------------------------------------------------------------- Windows

fn windows() {
    for knob in [
        "LIBPLACEBO_PREFIX",
        "FFMPEG_DIR",
        "VULKAN_SDK",
        "LIBPLACEBO_SPLAT",
        "LIBCLANG_PATH",
    ] {
        println!("cargo:rerun-if-env-changed={knob}");
    }

    let placebo = prefix("LIBPLACEBO_PREFIX", DEFAULT_PLACEBO);
    let ffmpeg = prefix("FFMPEG_DIR", DEFAULT_FFMPEG);
    let vulkan = prefix("VULKAN_SDK", DEFAULT_VULKAN);
    let splat = prefix("LIBPLACEBO_SPLAT", DEFAULT_SPLAT);

    let includes = Includes {
        placebo: placebo.join("include"),
        ffmpeg: ffmpeg.join("include"),
        vulkan: vulkan.join("Include"),
        crt: splat
            .join("VC/Tools/MSVC")
            .join(latest(&splat.join("VC/Tools/MSVC")))
            .join("include"),
        ucrt: sdk_include(&splat, "ucrt"),
        um: sdk_include(&splat, "um"),
        shared: sdk_include(&splat, "shared"),
    };

    bindings(&includes.all());
    shim(&includes.all());
    if feature("libav") {
        glue(&includes.all(), "clang-cl");
    }
    link(&placebo);
}

/// Every include directory the bindings and the native shims need, in one place.
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
    /// Everything `cc` needs to compile C for this ABI.
    fn all(&self) -> [PathBuf; 7] {
        [
            self.placebo.clone(),
            self.ffmpeg.clone(),
            self.vulkan.clone(),
            self.crt.clone(),
            self.ucrt.clone(),
            self.um.clone(),
            self.shared.clone(),
        ]
    }
}

// ------------------------------------------------------------------ Linux

fn linux() {
    println!("cargo:rerun-if-env-changed=LIBPLACEBO_PREFIX");
    println!("cargo:rerun-if-env-changed=LIBCLANG_PATH");

    let mut includes = match env::var_os("LIBPLACEBO_PREFIX") {
        Some(prefix) => {
            let prefix = PathBuf::from(prefix);
            assert!(
                prefix.is_dir(),
                "LIBPLACEBO_PREFIX points at {}, which is not a directory",
                prefix.display()
            );
            let lib = prefix.join("lib");
            // The prefix may sit in `lib` or `lib64`.
            for dir in [lib.clone(), prefix.join("lib64")] {
                if dir.is_dir() {
                    println!("cargo:rustc-link-search=native={}", dir.display());
                }
            }
            println!("cargo:rustc-link-lib=libplacebo");
            vec![prefix.join("include")]
        }
        None => probe("libplacebo").include_paths,
    };

    // The public headers include `vulkan/vulkan.h`; pkg-config knows where the
    // distribution keeps it, and clang usually finds it anyway.
    if let Ok(vulkan) = pkg_config::Config::new().probe("vulkan") {
        includes.extend(vulkan.include_paths);
    }

    // libclang finds its own builtin headers (`stdbool.h`, `stdint.h`) from a
    // resource directory derived from the library's path, and on Fedora that
    // derivation misses: libclang.so.22.1 looks for `clang/22.1.8` while the
    // package installs `clang/22`. Ask the driver instead of guessing.
    includes.extend(clang_include_dirs());

    bindings(&includes);
    shim(&includes);
    if feature("libav") {
        // libplacebo's libav glue includes the FFmpeg headers, which the caller
        // is responsible for having (libavcodec-devel or an FFMPEG_DIR-style
        // prefix). Probe rather than guess: a missing one is a clear error here
        // instead of 200 lines of clang diagnostics.
        for package in ["libavutil", "libavcodec", "libavformat"] {
            includes.extend(probe(package).include_paths);
        }
        glue(&includes, "cc");
    }
}

/// A pkg-config lookup whose failure says what to install.
fn probe(package: &str) -> pkg_config::Library {
    pkg_config::Config::new().probe(package).unwrap_or_else(|error| {
        panic!("pkg-config could not find {package}: {error}; install its -devel package")
    })
}

/// clang's own include directory, the one holding `stdbool.h` and friends.
fn clang_include_dirs() -> Vec<PathBuf> {
    let Ok(output) = std::process::Command::new("clang")
        .arg("-print-resource-dir")
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let dir = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let include = dir.join("include");
    if include.is_dir() { vec![include] } else { Vec::new() }
}

// ------------------------------------------------------------------ shared

/// Generates the bindings from the installed headers.
fn bindings(includes: &[PathBuf]) {
    for dir in includes {
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
    for dir in includes {
        builder = builder.clang_arg(format!("-I{}", dir.display()));
    }
    builder
        .generate()
        .expect("bindgen could not read libplacebo's headers")
        .write_to_file(out_dir().join("bindings.rs"))
        .expect("could not write bindings.rs");
}

/// Compiles `shim.c`, which turns the versioned `pl_log_create` macro into one
/// symbol that works against any installed libplacebo.
fn shim(includes: &[PathBuf]) {
    let mut build = cc::Build::new();
    build.file("src/shim.c").warnings(false);
    for dir in includes {
        build.include(dir);
    }
    build.compile("placebo_shim");
}

/// Compiles `libav.c`, which is the only part of libplacebo's libav support that
/// is not `static inline`.
fn glue(includes: &[PathBuf], compiler: &str) {
    let mut build = cc::Build::new();
    build.compiler(compiler).file("src/libav.c").warnings(false);
    for dir in includes {
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

/// Whether a cargo feature is on, as the build script sees it.
fn feature(name: &str) -> bool {
    let key = format!("CARGO_FEATURE_{}", name.to_uppercase().replace('-', "_"));
    env::var_os(key).is_some()
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
