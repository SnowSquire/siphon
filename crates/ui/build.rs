//! Points the linker and the loader at a libplacebo built into this build's
//! tree.
//!
//! A source-built libplacebo lives under `OUT_DIR` (or `LIBPLACEBO_BUILD_DIR`),
//! which is not a default search path. `libplacebo-sys` passes the libdir as
//! `links` metadata; the search path keeps another dependency's system `-L`
//! directory from shadowing it at link time, and a shared build also gets an
//! rpath so a binary started directly works without `LD_LIBRARY_PATH`. A system
//! or prefix libplacebo sets nothing, and Windows copies the DLL next to the
//! executable instead.
//!
//! It also fetches the UI's brand face, Chillax. The ITF Free Font License
//! allows embedding the font in the application but forbids distributing the
//! font file through a repository, so it is never committed: the build
//! downloads Fontshare's archive and writes the variable TTF into the profile's
//! `OUT_DIR` for `include_bytes!`. Because that lives under `target/`, a
//! `cargo clean` drops it and the next build fetches it again.
//! `SIPHON_CHILLAX_TTF` points the build at a local copy instead.
//!
//! On the Windows target it also links the brand icon into the executable as a
//! resource. The shell reads a program's icon from there and nowhere else:
//! Explorer's file icon, the shortcut the installer writes, a pinned task-bar
//! button and the Apps & features entry are all the image, not the window.

use std::env;
use std::fs;
use std::io::Read as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    font();
    icon();
    println!("cargo:rerun-if-env-changed=DEP_PLACEBO_LIBDIR");
    println!("cargo:rerun-if-env-changed=DEP_PLACEBO_RPATH");
    if env::var("CARGO_CFG_TARGET_FAMILY").as_deref() != Ok("unix") {
        return;
    }
    if let Some(dir) = env::var_os("DEP_PLACEBO_LIBDIR")
        && !dir.is_empty()
    {
        println!(
            "cargo:rustc-link-search=native={}",
            Path::new(&dir).display()
        );
    }
    if let Some(dir) = env::var_os("DEP_PLACEBO_RPATH")
        && !dir.is_empty()
    {
        println!(
            "cargo:rustc-link-arg=-Wl,-rpath,{}",
            Path::new(&dir).display()
        );
    }
}

/// Embeds the brand icon in the executable as a Windows resource.
///
/// `crates/ui/icons/icon.ico` is already what the running window sets its
/// title-bar and task-bar icon from, but the shell does not ask a running
/// program for the icon of its *file*: Explorer, a shortcut, a pinned button
/// and Apps & features read `RT_GROUP_ICON` out of the image. Without this the
/// executable has the generic application icon and a pinned button has no logo.
///
/// The script is one group icon, because the `.ico` already carries every size
/// Windows asks for. `llvm-rc` is the resource compiler the cross build carries
/// (`nix/msvc.nix`); `rc.exe` is the fallback for a native MSVC build, and `RC`
/// names another. The `.res` goes on the link line as one more input, which
/// both linkers accept.
///
/// MSVC only: a GNU link wants the resource assembled into a COFF object
/// first, and this workspace builds no GNU Windows target.
fn icon() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return;
    }
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let icon = manifest.join("icons").join("icon.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-env-changed=RC");

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let script = out.join("siphon.rc");
    let resource = out.join("siphon.res");
    // Quotes keep a source path with a space from ending the file name early;
    // forward slashes are accepted by both compilers.
    let text = format!("1 ICON \"{}\"\n", icon.display());
    fs::write(&script, text).unwrap_or_else(|error| panic!("{}: {error}", script.display()));

    let mut compilers = Vec::with_capacity(3);
    if let Some(named) = env::var_os("RC") {
        compilers.push(named);
    }
    compilers.push("llvm-rc".into());
    // A native Windows build with the MSVC toolset has `rc.exe`, and `llvm-rc`
    // only when LLVM is installed; the cross build has `llvm-rc` alone.
    if cfg!(windows) {
        compilers.push("rc".into());
    }
    let mut failure = String::from("no resource compiler ran");
    for compiler in &compilers {
        let outcome = Command::new(compiler)
            .arg("/fo")
            .arg(&resource)
            .arg(&script)
            .status();
        match outcome {
            Ok(status) if status.success() => {
                println!("cargo:rustc-link-arg-bins={}", resource.display());
                return;
            }
            Ok(status) => failure = format!("{}: {status}", compiler.to_string_lossy()),
            Err(error) => failure = format!("{}: {error}", compiler.to_string_lossy()),
        }
    }
    panic!(
        "could not compile {} into an icon resource ({failure}); set RC to an rc compiler",
        script.display()
    );
}

/// Fontshare's archive for the whole family.
const CHILLAX_URL: &str = "https://api.fontshare.com/v2/fonts/download/chillax";
/// The one entry the UI needs: the unmodified variable TTF.
const CHILLAX_ENTRY: &str = "Chillax_Complete/Fonts/TTF/Chillax-Variable.ttf";

/// Puts `Chillax-Variable.ttf` in `OUT_DIR` so `include_bytes!` can reach it.
fn font() {
    println!("cargo:rerun-if-env-changed=SIPHON_CHILLAX_TTF");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo"))
        .join("Chillax-Variable.ttf");
    if out.is_file() {
        // This profile already fetched the font; the script just reran because
        // some crate source changed.
        return;
    }
    let bytes = env::var_os("SIPHON_CHILLAX_TTF").map_or_else(
        || {
            download().unwrap_or_else(|error| {
                panic!(
                    "could not fetch Chillax from {CHILLAX_URL}: {error}\n\
                     set SIPHON_CHILLAX_TTF to a local Chillax-Variable.ttf to build offline"
                )
            })
        },
        |path| {
            let path = PathBuf::from(path);
            fs::read(&path)
                .unwrap_or_else(|error| panic!("SIPHON_CHILLAX_TTF {}: {error}", path.display()))
        },
    );
    fs::write(&out, bytes).unwrap_or_else(|error| panic!("{}: {error}", out.display()));
}

fn download() -> Result<Vec<u8>, String> {
    let mut response = ureq::get(CHILLAX_URL)
        .call()
        .map_err(|error| format!("GET: {error}"))?;
    let archive = response
        .body_mut()
        .read_to_vec()
        .map_err(|error| format!("read: {error}"))?;
    zip_entry(&archive, CHILLAX_ENTRY)
}

/// Pulls one stored or deflated file out of a zip archive.
///
/// The whole archive is a few hundred KiB and this runs once per profile, so a
/// linear scan of the central directory is enough; zip64, encryption and
/// compression methods other than store/deflate are rejected.
fn zip_entry(archive: &[u8], wanted: &str) -> Result<Vec<u8>, String> {
    const END: u32 = 0x0605_4b50;
    const CENTRAL: u32 = 0x0201_4b50;
    const LOCAL: u32 = 0x0403_4b50;
    let truncated = || "truncated zip".to_owned();
    let end = archive
        .windows(4)
        .rposition(|window| u32_at(window, 0) == Some(END))
        .ok_or_else(|| "no zip end record".to_owned())?;
    let entries = u16_at(archive, end + 10).ok_or_else(truncated)?;
    let mut at = u32_at(archive, end + 16).ok_or_else(truncated)? as usize;
    for _ in 0..entries {
        if u32_at(archive, at) != Some(CENTRAL) {
            return Err("bad central directory".to_owned());
        }
        let method = u16_at(archive, at + 10).ok_or_else(truncated)?;
        let compressed = u32_at(archive, at + 20).ok_or_else(truncated)? as usize;
        let size = u32_at(archive, at + 24).ok_or_else(truncated)? as usize;
        let name_len = u16_at(archive, at + 28).ok_or_else(truncated)? as usize;
        let extra_len = u16_at(archive, at + 30).ok_or_else(truncated)? as usize;
        let comment_len = u16_at(archive, at + 32).ok_or_else(truncated)? as usize;
        let local = u32_at(archive, at + 42).ok_or_else(truncated)? as usize;
        let name = archive
            .get(at + 46..at + 46 + name_len)
            .ok_or_else(truncated)?;
        at += 46 + name_len + extra_len + comment_len;
        if name != wanted.as_bytes() {
            continue;
        }
        if u32_at(archive, local) != Some(LOCAL) {
            return Err("bad local header".to_owned());
        }
        let local_name = u16_at(archive, local + 26).ok_or_else(truncated)? as usize;
        let local_extra = u16_at(archive, local + 28).ok_or_else(truncated)? as usize;
        let start = local + 30 + local_name + local_extra;
        let data = archive
            .get(start..start + compressed)
            .ok_or_else(truncated)?;
        let mut font = Vec::with_capacity(size);
        match method {
            0 => font.extend_from_slice(data),
            8 => {
                flate2::read::DeflateDecoder::new(data)
                    .read_to_end(&mut font)
                    .map_err(|error| format!("inflate: {error}"))?;
            }
            other => return Err(format!("unsupported zip compression {other}")),
        }
        if font.len() != size {
            return Err(format!("size mismatch: {} != {size}", font.len()));
        }
        return Ok(font);
    }
    Err(format!("{wanted} not in archive"))
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    bytes
        .get(at..at + 2)?
        .try_into()
        .ok()
        .map(u16::from_le_bytes)
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at + 4)?
        .try_into()
        .ok()
        .map(u32::from_le_bytes)
}
