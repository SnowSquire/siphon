//! Points the linker and the loader at a libplacebo built into this build's
//! tree.
//!
//! A source-built libplacebo lives under `OUT_DIR` (or `LIBPLACEBO_BUILD_DIR`),
//! which is not a default search path. `libplacebo-sys` forwards the libdir as
//! `links` metadata; the search path keeps another dependency's system `-L`
//! directory from shadowing it at link time, and a shared build also gets an
//! rpath so a binary started directly works without `LD_LIBRARY_PATH`. A system
//! or prefix libplacebo sets nothing, and Windows copies the DLL next to the
//! executable instead.

use std::env;
use std::path::Path;

fn main() {
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
