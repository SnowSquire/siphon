//! Builds the bindings, the libav glue, the `pl_log_create` shim, the link
//! directives and (on Windows) the DLL copy for the libplacebo this repository
//! links.
//!
//! `LIBPLACEBO_SOURCE` selects where the library comes from:
//!
//! * `system` — pkg-config. The default on Linux, where libplacebo is a
//!   distribution package.
//! * `prefix` — a `meson install` prefix named by `LIBPLACEBO_PREFIX`. The
//!   default on Windows, whose scoop packages carry no pkg-config files.
//! * `build` — meson builds the checkout at `LIBPLACEBO_SRC` into the Cargo
//!   output directory, so a machine with no libplacebo can still link one.
//! * `auto` — `system`, falling back to `build` when pkg-config has no
//!   libplacebo.
//!
//! `LIBPLACEBO_LINK` picks `dynamic` (the default) or `static` for a source
//! build. `LIBPLACEBO_BUILD_DIR` moves the meson build and install trees out of
//! `OUT_DIR` when a checkout is built repeatedly.
//!
//! Discovery is declarative (`[package.metadata.system-deps]` in `Cargo.toml`)
//! and belongs to `system-deps`: pkg-config on Linux, its environment-variable
//! overrides for the Windows prefix (which ships no pkg-config files), and the
//! build-internal hook when the library has to be compiled. The meson
//! invocation that used to be `scripts/build-libplacebo.sh` lives in
//! `build_with_meson` below, so a source build needs no separate shell step and
//! works on both platforms.

use std::collections::HashMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The user's home directory, as the scoop/splat/vulkan defaults are relative to
/// it. Windows always sets `USERPROFILE`; `HOME` covers the MSYS shell.
fn home() -> PathBuf {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// `meson install`'s prefix for the libplacebo this repo builds.
fn default_placebo() -> PathBuf {
    home().join(".local/libplacebo-shared")
}

/// Scoop's root: `%SCOOP%`, else `~/scoop` (scoop's own default).
fn scoop() -> PathBuf {
    env::var_os("SCOOP").map_or_else(|| home().join("scoop"), PathBuf::from)
}

/// The scoop `FFmpeg` package.
fn default_ffmpeg() -> PathBuf {
    scoop().join("apps/ffmpeg-shared/current")
}

/// The Vulkan SDK, for `vulkan/vulkan.h`.
fn default_vulkan() -> PathBuf {
    scoop().join("apps/vulkan/current")
}

/// The xwin splat holding the MSVC CRT and Windows SDK: `%XWIN_SYS_ROOT%`, else
/// xwin's own cache default.
fn default_splat() -> PathBuf {
    env::var_os("XWIN_SYS_ROOT").map_or_else(|| home().join(".xwin-cache/splat"), PathBuf::from)
}

fn main() {
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=src/shim.c");
    println!("cargo:rerun-if-changed=src/libav.c");
    for knob in [
        "LIBPLACEBO_SOURCE",
        "LIBPLACEBO_LINK",
        "LIBPLACEBO_SRC",
        "LIBPLACEBO_BUILD_DIR",
        "LIBPLACEBO_PREFIX",
        "FFMPEG_DIR",
        "VULKAN_SDK",
        "LIBPLACEBO_SPLAT",
        "LIBCLANG_PATH",
        // The defaults below are relative to these.
        "SCOOP",
        "XWIN_SYS_ROOT",
        "USERPROFILE",
        "HOME",
    ] {
        println!("cargo:rerun-if-env-changed={knob}");
    }
    // Build scripts run on the host but serve the *target*: `cfg(windows)` here
    // would describe the host, so cross-compiling needs Cargo's target variable.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows();
    } else {
        linux();
    }
}

// --------------------------------------------------------------- selection

/// Where libplacebo comes from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    System,
    Prefix,
    Build,
    Auto,
}

impl Source {
    fn from_env(default: Self) -> Self {
        match env::var("LIBPLACEBO_SOURCE") {
            Ok(value) => match value.as_str() {
                "system" => Self::System,
                "prefix" => Self::Prefix,
                "build" => Self::Build,
                "auto" => Self::Auto,
                other => {
                    panic!("LIBPLACEBO_SOURCE={other:?}; expected system, prefix, build or auto")
                }
            },
            Err(_) if env::var_os("LIBPLACEBO_PREFIX").is_some() => Self::Prefix,
            Err(_) => default,
        }
    }
}

/// How the library is linked.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Link {
    Dynamic,
    Static,
}

impl Link {
    fn from_env() -> Self {
        match env::var("LIBPLACEBO_LINK").as_deref() {
            Ok("static") => Self::Static,
            Ok("dynamic") | Err(_) => Self::Dynamic,
            Ok(other) => panic!("LIBPLACEBO_LINK={other:?}; expected dynamic or static"),
        }
    }

    const fn meson_default_library(self) -> &'static str {
        match self {
            Self::Dynamic => "shared",
            Self::Static => "static",
        }
    }

    /// The marker written next to the meson build tree.
    const fn key(self) -> &'static str {
        match self {
            Self::Dynamic => "dynamic",
            Self::Static => "static",
        }
    }
}

/// Tell system-deps whether it may build libplacebo, and report whether a
/// build-internal hook is needed. An explicit
/// `SYSTEM_DEPS_LIBPLACEBO_BUILD_INTERNAL` from the caller wins over
/// `LIBPLACEBO_SOURCE`.
fn configure_system_deps(source: Source, link: Link) -> bool {
    let default = match source {
        Source::System | Source::Prefix => "never",
        Source::Build => "always",
        Source::Auto => "auto",
    };
    let requested = env::var("SYSTEM_DEPS_LIBPLACEBO_BUILD_INTERNAL").ok();
    let mode = requested.as_deref().unwrap_or(default);

    set_env_default("SYSTEM_DEPS_LIBPLACEBO_BUILD_INTERNAL", default);
    if link == Link::Static {
        set_env_default("SYSTEM_DEPS_LIBPLACEBO_LINK", "static");
    }
    matches!(mode, "always" | "auto")
}

/// Names a prefix through system-deps' environment-variable override, which is
/// how a pkg-config-less platform (Windows) still goes through one discovery
/// path.
fn use_prefix(prefix: &Path, lib: &str) {
    let dirs: Vec<PathBuf> = ["lib", "lib64"]
        .iter()
        .map(|part| prefix.join(part))
        .filter(|dir| dir.is_dir())
        .collect();
    assert!(
        !dirs.is_empty(),
        "no lib or lib64 directory under {}",
        prefix.display()
    );
    let search = env::join_paths(&dirs).expect("lib directories are valid paths");

    set_env("SYSTEM_DEPS_LIBPLACEBO_NO_PKG_CONFIG", "1");
    set_env("SYSTEM_DEPS_LIBPLACEBO_SEARCH_NATIVE", search);
    set_env("SYSTEM_DEPS_LIBPLACEBO_INCLUDE", prefix.join("include"));
    set_env("SYSTEM_DEPS_LIBPLACEBO_LIB", lib);
}

/// Resolves libplacebo through system-deps, with the meson build registered as
/// the internal fallback.
fn probe(source: Source, link: Link, lib: &'static str) -> system_deps::Dependencies {
    let may_build = configure_system_deps(source, link);

    let mut config = system_deps::Config::new();
    if may_build {
        config = config.add_build_internal("libplacebo", move |name, version| {
            build_internal(name, version, link, lib)
        });
    }
    config.probe().unwrap_or_else(|error| {
        panic!(
            "system-deps could not resolve libplacebo: {error}\n\
             install the platform's libplacebo development package, or set \
             LIBPLACEBO_SOURCE=build with LIBPLACEBO_SRC pointing at a checkout"
        )
    })
}

// ------------------------------------------------------------------ Linux

fn linux() {
    let source = Source::from_env(Source::System);
    let link = Link::from_env();

    if source == Source::Prefix {
        let prefix = env::var_os("LIBPLACEBO_PREFIX").map_or_else(
            || panic!("LIBPLACEBO_SOURCE=prefix needs LIBPLACEBO_PREFIX to name one"),
            PathBuf::from,
        );
        assert!(
            prefix.is_dir(),
            "LIBPLACEBO_PREFIX points at {}, which is not a directory",
            prefix.display()
        );
        use_prefix(&prefix, "placebo");
    }

    let deps = probe(source, link, "placebo");

    let mut includes: Vec<PathBuf> = deps.all_include_paths().into_iter().cloned().collect();
    // The public headers include `vulkan/vulkan.h`; pkg-config knows where the
    // distribution keeps it, and clang usually finds it anyway.
    includes.extend(clang_include_dirs());
    let defines = deps.all_defines();

    bindings(&includes, &defines);
    shim(&includes, &defines);
    if feature("libav") {
        // libplacebo's libav glue includes the FFmpeg headers, which the caller
        // is responsible for having (libavcodec-devel or an FFMPEG_DIR-style
        // prefix). system-deps probes them, so a missing one is a clear error
        // instead of 200 lines of clang diagnostics.
        glue(&includes, &defines, "cc");
    }

    forward_libdir(&deps, link, source);
}

/// Tells the dependent where a source-built libplacebo landed.
///
/// The link-time directory matters even for a static build: another dependency
/// can put a system `-L` directory ahead of the source prefix, and then
/// `-lplacebo`/`-l static=placebo` would pick the distro's copy. A shared build
/// also needs an rpath so a directly started binary finds it.
fn forward_libdir(deps: &system_deps::Dependencies, link: Link, source: Source) {
    let Some(dir) = deps
        .get_by_name("libplacebo")
        .and_then(|lib| lib.link_paths.first())
    else {
        return;
    };
    // `auto` may have settled on the system library, in which case there is
    // nothing to forward.
    if source != Source::Build && !dir.starts_with(build_root()) {
        return;
    }
    // Unknown `cargo:key=value` lines are `links` metadata; the UI's build
    // script reads them as `DEP_PLACEBO_LIBDIR` and `DEP_PLACEBO_RPATH`.
    println!("cargo:libdir={}", dir.display());
    if link == Link::Dynamic {
        println!("cargo:rpath={}", dir.display());
    }
}

// ---------------------------------------------------------------- Windows

fn windows() {
    let source = Source::from_env(Source::Prefix);
    let link = Link::from_env();

    if source == Source::Prefix {
        use_prefix(&prefix("LIBPLACEBO_PREFIX", default_placebo()), "libplacebo");
    }

    let deps = probe(source, link, "libplacebo");

    let placebo = deps
        .get_by_name("libplacebo")
        .and_then(|lib| lib.include_paths.first())
        .cloned()
        .unwrap_or_else(|| panic!("libplacebo resolved without an include path"));
    let ffmpeg = prefix("FFMPEG_DIR", default_ffmpeg());
    let vulkan = prefix("VULKAN_SDK", default_vulkan());
    let splat = prefix("LIBPLACEBO_SPLAT", default_splat());

    let includes = Includes {
        placebo,
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
    let defines = deps.all_defines();

    bindings(&includes.all(), &defines);
    shim(&includes.all(), &defines);
    if feature("libav") {
        glue(&includes.all(), &defines, "clang-cl");
    }

    if link == Link::Dynamic
        && let Some(prefix) = dll_prefix(source, &deps)
    {
        copy_dll(&prefix);
    }
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

/// The install prefix holding the DLL, for the modes that own one.
fn dll_prefix(source: Source, deps: &system_deps::Dependencies) -> Option<PathBuf> {
    let prefix = deps
        .get_by_name("libplacebo")
        .and_then(|lib| lib.link_paths.first())
        .and_then(|lib| lib.parent())?;
    if source == Source::Prefix || source == Source::Build || prefix.starts_with(build_root()) {
        Some(prefix.to_path_buf())
    } else {
        // `system`: the loader has to find the DLL itself.
        None
    }
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

// -------------------------------------------------------------- meson build

/// A `meson install` tree built by this script.
struct Installed {
    prefix: PathBuf,
}

impl Installed {
    fn include(&self) -> PathBuf {
        self.prefix.join("include")
    }

    fn lib(&self) -> PathBuf {
        self.prefix.join("lib")
    }

    fn pc_dir(&self) -> PathBuf {
        self.lib().join("pkgconfig")
    }

    /// A dynamic library described by hand: `Library::from_internal_pkg_config`
    /// probes with `--static`, which would pull libplacebo's private
    /// dependencies into a shared link that does not need them.
    fn dynamic_library(&self, name: &str, link_name: &str, version: &str) -> system_deps::Library {
        system_deps::Library {
            name: name.to_owned(),
            source: system_deps::Source::PkgConfig,
            libs: vec![system_deps::InternalLib {
                name: link_name.to_owned(),
                is_static_available: false,
            }],
            link_paths: vec![self.lib()],
            link_files: Vec::new(),
            frameworks: Vec::new(),
            framework_paths: Vec::new(),
            include_paths: vec![self.include()],
            ld_args: Vec::new(),
            defines: HashMap::new(),
            version: version.to_owned(),
            statik: false,
        }
    }
}

/// The build-internal hook system-deps calls when pkg-config has no libplacebo
/// (or `LIBPLACEBO_SOURCE=build` says to ignore it).
fn build_internal(
    name: &str,
    version: &str,
    link: Link,
    link_name: &'static str,
) -> Result<system_deps::Library, system_deps::BuildInternalClosureError> {
    let installed =
        build_with_meson(link).map_err(system_deps::BuildInternalClosureError::Failed)?;

    if link == Link::Dynamic {
        return Ok(installed.dynamic_library(name, link_name, version));
    }

    let mut library =
        system_deps::Library::from_internal_pkg_config(installed.pc_dir(), name, version).map_err(
            |error| {
                system_deps::BuildInternalClosureError::Failed(format!(
                    "could not read the pkg-config file meson installed: {error}"
                ))
            },
        )?;
    // MSVC names its libraries `libplacebo.lib`, while meson's `.pc` says
    // `-lplacebo`.
    if link_name != "placebo" {
        for lib in &mut library.libs {
            if lib.name == "placebo" {
                link_name.clone_into(&mut lib.name);
            }
        }
    } else if !library.libs.iter().any(|lib| lib.name == "stdc++") {
        // libplacebo's C++ objects (`convert.cc`) use `std::to_chars` and
        // friends. A shared build pulls the runtime in through the library's
        // `DT_NEEDED`; meson's `.pc` has no reason to name it, so a static
        // build has to.
        library.libs.push(system_deps::InternalLib {
            name: "stdc++".to_owned(),
            is_static_available: false,
        });
    }
    Ok(library)
}

/// The meson build and install tree, reused across build-script runs.
fn build_root() -> PathBuf {
    env::var_os("LIBPLACEBO_BUILD_DIR").map_or_else(|| out_dir().join("libplacebo"), PathBuf::from)
}

/// Runs meson over `LIBPLACEBO_SRC` and installs into
/// `LIBPLACEBO_BUILD_DIR/prefix`.
fn build_with_meson(link: Link) -> Result<Installed, String> {
    let source = env::var_os("LIBPLACEBO_SRC")
        .map(PathBuf::from)
        .ok_or_else(|| {
            "LIBPLACEBO_SOURCE builds libplacebo from source, but LIBPLACEBO_SRC is not set.\n\
             Point it at a libplacebo checkout, or set LIBPLACEBO_SOURCE=system."
                .to_owned()
        })?;
    let root = build_root();
    let build = root.join("build");
    let prefix = root.join("prefix");
    let scratch = root.join("meson");
    fs::create_dir_all(&scratch)
        .map_err(|error| format!("cannot create {}: {error}", scratch.display()))?;

    // Build-script stderr, not `cargo:warning`: Cargo replays cached warnings
    // for a fresh unit, which would claim a rebuild on every later build.
    eprintln!(
        "building libplacebo from {} (this takes a few minutes)",
        source.display()
    );
    for watched in [source.join("meson.build"), source.join("src")] {
        if watched.exists() {
            println!("cargo:rerun-if-changed={}", watched.display());
        }
    }

    let configured = build.join("meson-private/coredata.dat").is_file();
    let previous = fs::read_to_string(root.join("link")).ok();
    let wipe =
        build.exists() && (!configured || previous.as_deref().map(str::trim) != Some(link.key()));

    let mut args: Vec<OsString> = vec!["setup".into()];
    if wipe {
        args.push("--wipe".into());
    } else if configured {
        args.push("--reconfigure".into());
    }
    args.push(build.as_os_str().to_owned());
    args.push(source.as_os_str().to_owned());
    for option in [
        "--buildtype=release".to_owned(),
        format!("--default-library={}", link.meson_default_library()),
        format!("--prefix={}", meson_path(&prefix)),
        "--libdir=lib".to_owned(),
        "-Dvulkan=enabled".to_owned(),
        "-Dopengl=disabled".to_owned(),
        "-Dd3d11=disabled".to_owned(),
        "-Dshaderc=enabled".to_owned(),
        "-Dglslang=disabled".to_owned(),
        "-Ddovi=disabled".to_owned(),
        "-Dlibdovi=disabled".to_owned(),
        "-Dlcms=disabled".to_owned(),
        "-Dxxhash=disabled".to_owned(),
        "-Ddemos=false".to_owned(),
        "-Dtests=false".to_owned(),
        "-Dbench=false".to_owned(),
        "-Dfuzz=false".to_owned(),
    ] {
        args.push(option.into());
    }

    let mut environment = Vec::new();
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_meson(&source, &scratch, &mut args, &mut environment)?;
    }

    run("meson setup", &args, &environment)?;
    run(
        "meson compile",
        &["compile".into(), "-C".into(), build.as_os_str().to_owned()],
        &environment,
    )?;
    run(
        "meson install",
        &[
            "install".into(),
            "--only-changed".into(),
            "-C".into(),
            build.as_os_str().to_owned(),
        ],
        &environment,
    )?;
    fs::write(root.join("link"), link.key())
        .map_err(|error| format!("cannot record the link mode: {error}"))?;

    Ok(Installed { prefix })
}

/// Writes the two files and the environment meson needs on Windows.
///
/// The Vulkan SDK ships shaderc as a static library with no pkg-config file, so
/// one is written for it; Windows pkgconf rewrites a `.pc`'s `prefix` from the
/// file's own location, so `PKG_CONFIG_DONT_DEFINE_PREFIX=1` is required.
/// `INCLUDE` there must not contain the Vulkan SDK: meson drops a pkg-config
/// include directory that is already in the environment, which silently loses
/// `shaderc/shaderc.h`.
fn windows_meson(
    source: &Path,
    scratch: &Path,
    args: &mut Vec<OsString>,
    environment: &mut Vec<(String, OsString)>,
) -> Result<(), String> {
    let vulkan = prefix("VULKAN_SDK", default_vulkan());
    let splat = prefix("LIBPLACEBO_SPLAT", default_splat());
    let msvc = splat.join("VC/Tools/MSVC");
    let sdk = splat.join("Windows Kits/10");

    let shaderc = format!(
        "prefix={}\n\
         includedir=${{prefix}}/Include\n\
         libdir=${{prefix}}/Lib\n\
         \n\
         Name: shaderc\n\
         Description: The shaderc library\n\
         Version: 2025.1\n\
         Cflags: -I${{includedir}}\n\
         Libs: -L${{libdir}} -lshaderc_combined\n",
        meson_path(&vulkan)
    );
    let shaderc_pc = scratch.join("shaderc.pc");
    fs::write(&shaderc_pc, shaderc)
        .map_err(|error| format!("cannot write {}: {error}", shaderc_pc.display()))?;

    let native = format!(
        "[binaries]\n\
         c = 'clang-cl'\n\
         cpp = 'clang-cl'\n\
         ar = 'llvm-lib'\n\
         strip = 'llvm-strip'\n\
         pkg-config = '{}'\n",
        env::var("PKG_CONFIG").unwrap_or_else(|_| "pkg-config".to_owned())
    );
    let native_file = scratch.join("native.ini");
    fs::write(&native_file, native)
        .map_err(|error| format!("cannot write {}: {error}", native_file.display()))?;

    let version = latest(&msvc);
    let sdk_version = latest(&sdk.join("Include"));
    let include = join_windows_paths([
        msvc.join(&version).join("include"),
        sdk.join("Include").join(&sdk_version).join("ucrt"),
        sdk.join("Include").join(&sdk_version).join("um"),
        sdk.join("Include").join(&sdk_version).join("shared"),
    ]);
    let library = join_windows_paths([
        msvc.join(&version).join("lib/x86_64"),
        sdk.join("Lib").join(&sdk_version).join("ucrt/x86_64"),
        sdk.join("Lib").join(&sdk_version).join("um/x86_64"),
    ]);
    let mut path = vec![vulkan.join("Bin")];
    if let Some(existing) = env::var_os("PATH") {
        path.extend(env::split_paths(&existing));
    }
    let mut pkg_config_path = vec![scratch.to_path_buf()];
    if let Some(existing) = env::var_os("PKG_CONFIG_PATH") {
        pkg_config_path.extend(env::split_paths(&existing));
    }

    environment.push((
        "PKG_CONFIG_PATH".into(),
        join_windows_paths(pkg_config_path),
    ));
    environment.push(("PKG_CONFIG_DONT_DEFINE_PREFIX".into(), "1".into()));
    environment.push(("INCLUDE".into(), include));
    environment.push(("LIB".into(), library));
    environment.push(("PATH".into(), join_windows_paths(path)));

    args.push(format!("--native-file={}", meson_path(&native_file)).into());
    args.push(
        format!(
            "-Dvulkan-registry={}",
            meson_path(&source.join("3rdparty/Vulkan-Headers/registry/vk.xml"))
        )
        .into(),
    );
    args.push(format!("-Dvulkan-sdk={}", meson_path(&vulkan)).into());
    Ok(())
}

fn run(what: &str, args: &[OsString], environment: &[(String, OsString)]) -> Result<(), String> {
    let mut command = Command::new("meson");
    command.args(args);
    for (key, value) in environment {
        command.env(key, value);
    }
    let output = command.output().map_err(|error| {
        format!(
            "could not run meson: {error}\n\
             Install meson and ninja, or set LIBPLACEBO_SOURCE=system."
        )
    })?;
    if output.status.success() {
        return Ok(());
    }
    let mut message = format!("{what} failed with {}", output.status);
    append_output(&mut message, "stdout", &output.stdout);
    append_output(&mut message, "stderr", &output.stderr);
    Err(message)
}

fn append_output(message: &mut String, name: &str, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    if !text.is_empty() {
        let _ = write!(message, "\n{name}:\n{text}");
    }
}

/// Meson and pkg-config files want forward slashes even on Windows.
fn meson_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// `;` is the separator Windows uses for `INCLUDE`, `LIB` and `PATH`, whatever
/// the host this build script is running on.
fn join_windows_paths(paths: impl IntoIterator<Item = PathBuf>) -> OsString {
    let mut joined = OsString::new();
    for (index, path) in paths.into_iter().enumerate() {
        if index > 0 {
            joined.push(";");
        }
        joined.push(path);
    }
    joined
}

// ------------------------------------------------------------------ shared

/// `pkg-config`'s `-D` flags, which a static libplacebo needs (`PL_STATIC`).
type Defines<'a> = [(&'a str, &'a Option<String>)];

/// Generates the bindings from the installed headers.
fn bindings(includes: &[PathBuf], defines: &Defines<'_>) {
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
    for &(name, value) in defines {
        builder = builder.clang_arg(
            value
                .as_ref()
                .map_or_else(|| format!("-D{name}"), |value| format!("-D{name}={value}")),
        );
    }
    builder
        .generate()
        .expect("bindgen could not read libplacebo's headers")
        .write_to_file(out_dir().join("bindings.rs"))
        .expect("could not write bindings.rs");
}

/// Compiles `shim.c`, which turns the versioned `pl_log_create` macro into one
/// symbol that works against any installed libplacebo.
fn shim(includes: &[PathBuf], defines: &Defines<'_>) {
    let mut build = cc::Build::new();
    build.file("src/shim.c").warnings(false);
    for dir in includes {
        build.include(dir);
    }
    for &(name, value) in defines {
        build.define(name, value.as_deref());
    }
    build.compile("placebo_shim");
}

/// Compiles `libav.c`, which is the only part of libplacebo's libav support that
/// is not `static inline`.
fn glue(includes: &[PathBuf], defines: &Defines<'_>, compiler: &str) {
    let mut build = cc::Build::new();
    build.compiler(compiler).file("src/libav.c").warnings(false);
    for dir in includes {
        build.include(dir);
    }
    for &(name, value) in defines {
        build.define(name, value.as_deref());
    }
    build.compile("placebo_libav");
}

/// clang's own include directory, the one holding `stdbool.h` and friends.
///
/// libclang finds its builtin headers from a resource directory derived from the
/// library's path, and on Fedora that derivation misses: libclang.so.22.1 looks
/// for `clang/22.1.8` while the package installs `clang/22`. Ask the driver
/// instead of guessing.
fn clang_include_dirs() -> Vec<PathBuf> {
    let Ok(output) = Command::new("clang").arg("-print-resource-dir").output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let dir = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let include = dir.join("include");
    if include.is_dir() {
        vec![include]
    } else {
        Vec::new()
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
fn prefix(knob: &str, default: PathBuf) -> PathBuf {
    let value = env::var_os(knob).map_or(default, PathBuf::from);
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
    let mut versions: Vec<String> = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("cannot list {}: {error}", dir.display()))
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    // The splat names its directories by version (`14.44.35207`,
    // `10.0.22621.0`); lexicographic order puts `14.9` after `14.10`.
    versions.sort_by_cached_key(|version| version_key(version));
    versions
        .pop()
        .unwrap_or_else(|| panic!("no versions under {}", dir.display()))
}

/// Numeric components of a version directory name, so `14.10` sorts after
/// `14.9`. Component count is preserved: `14.44.1` > `14.44`.
fn version_key(version: &str) -> Vec<u64> {
    version
        .split(|character: char| !character.is_ascii_digit())
        .filter_map(|part| part.parse().ok())
        .collect()
}

/// Rebuilds the bindings when a header changes.
fn rerun_on_dir(dir: &Path) {
    println!("cargo:rerun-if-changed={}", dir.display());
}

/// Sets an environment variable for the rest of this build script.
fn set_env(key: &str, value: impl AsRef<OsStr>) {
    // SAFETY: build scripts are single-threaded and this runs before the first
    // child process, so no other thread can observe the environment
    // concurrently.
    unsafe { env::set_var(key, value) };
}

fn set_env_default(key: &str, value: &str) {
    if env::var_os(key).is_none() {
        set_env(key, value);
    }
}
