# Nix build

`flake.nix` builds everything the release ships, hermetically: the Linux daemon
and UI from nixpkgs' toolchain and libplacebo, and the Windows `siphon.exe` +
`siphon-daemon.exe` + the setup and its two zips, cross-compiled with clang-cl,
lld-link and the MSVC CRT/SDK. Every input has a hash, the release workflow
installs Nix and calls `nix build .#windows-installer`, and the shell scripts
that used to do this on a runner (`package-win.sh`, `cargo-win.sh`,
`build-shaderc-win.sh`, `build-libplacebo-win.sh`) are gone: one pipeline, not
two that had to be kept in agreement.

```sh
nix build .#linux              # siphon (Linux UI) + siphon-daemon in result/bin
nix build .#windows            # siphon.exe + siphon-daemon.exe + libplacebo-*.dll
nix build .#windows-installer  # the setup, the portable zip, the symbols zip
nix build                      # .#linux
nix develop                    # the Linux toolchain, without the -devel packages
nix develop .#windows          # the cross toolchain for cargo, targeting Windows
nix flake check                # evaluates and builds every output on this system
```

The flake targets `x86_64-linux` only: the Windows payload is cross-built *from*
Linux, so that is the one host that can produce both.

## What is pinned, and where

| input | pinned by |
|---|---|
| nixpkgs (LLVM, libplacebo, shaderc, meson, cmake for Linux) | `flake.lock`, bumped with `nix flake update` |
| Rust 1.100.0 and its Windows standard library | `rust-overlay` in `flake.lock`; the version is `rustVersion` in `flake.nix` |
| Cargo dependencies, including the `zgui` git workspace | `Cargo.lock` + `cargoLock.outputHashes` in `flake.nix` |
| Chillax (the UI's brand face) | `nix/sources.nix`: `fetchzip` of Fontshare's endpoint, hashed over the unpacked tree because the zip is generated per request |
| libplacebo source for the Windows prefix | `nix/sources.nix`: `fetchgit` at `LIBPLACEBO_REV` |
| shaderc's third-party checkouts | `nix/sources.nix`: a fixed-output derivation that runs `utils/git-sync-deps` once and records the tree |
| Visual Studio 18 channel manifest | `nix/sources.nix`: `fetchurl` of `aka.ms/vs/18/stable/channel` |
| MSVC CRT + Windows SDK payloads | `nix/sources.nix`: a fixed-output derivation around `xwin download`, pinned to SDK `10.0.28000` and toolset `14.52` |
| Vulkan headers | nixpkgs' `vulkan-headers`, staged into the SDK's `Include/` layout |

Nothing downloads during a build. The four fixed-output derivations are the only
place network access is allowed, and each one fails loudly if the bytes moved.

## The Windows build in pieces

`nix/msvc.nix` holds the cross environment and the two libraries that need it;
`nix/windows.nix` is the payload alone. In order:

1. **shaderc**, CMake with the `msvc-cross.cmake` toolchain file, target
   `shaderc_combined`, installed as a static archive plus a `.pc` that names it.
   libplacebo needs a GLSL compiler at run time, and linking it in keeps the
   payload to two exes and one DLL.
2. **libplacebo**, meson with `msvc-cross.ini`, the cross file carrying the
   `/imsvc` SDK includes and the `-libpath` import libraries (Vulkan on, GL/D3D11/glslang off, shaderc on, codecs and lcms off).
3. **Rust**, for `x86_64-pc-windows-msvc` with
   `CC_x86_64_pc_windows_msvc=clang-cl`, `lld-link` as the linker and the SDK
   import directories on `RUSTFLAGS`. `libplacebo-sys` is pointed at the prefix
   with `LIBPLACEBO_SOURCE=prefix`, which is also how it finds the DLL to copy
   next to the executable. The profile is the shipped one — fat LTO in a single
   codegen unit — so `.#windows` is slower than `cargo build --release` and is
   worth backgrounding.

The Rust step is three derivations, not one (`craneLib` in `flake.nix`):

| derivation | rebuilt when |
|---|---|
| `siphon-windows-deps` | `Cargo.lock`, rustc, the cross environment or the profile changes |
| `siphon-windows-ui` | plus anything under `crates/` the UI depends on |
| `siphon-windows-daemon` | plus anything under `crates/` the daemon depends on |

`buildDepsOnly` compiles the lockfile's dependencies against a dummy source
(crane also drops in a dummy build script, so build-dependencies are covered),
and `buildPackage` then builds one crate on top of those artifacts. Cargo is
still the resolver — feature unification, build scripts and `DEP_*` plumbing run
exactly as they would in a plain `cargo build` — which is the difference from
cargo2nix/crate2nix, where a generated `Cargo.nix` re-derives the graph.

The source is filtered to `Cargo.toml`, `Cargo.lock` and `crates/` (`lib.fileset`
in `flake.nix`), so editing a note, a workflow or this file does not invalidate
the payload; `crates/` carries the non-Rust files the crates embed — fonts,
icons, `index.hexpat`, and the C shim `libplacebo-sys` compiles. The installer
still takes the whole tree, because it needs the licence texts and the NSIS
script.

Four pieces are Nix-only plumbing worth knowing about:

- **LLVM 23.** The MSVC STL in the pinned toolset statically asserts
  `STL1000: Unexpected compiler version, expected Clang 22 or newer`, which is
  newer than nixpkgs' default `llvmPackages` (21), so the Windows build pins
  `llvmPackages_23`. This is the one pin that has to move together with the CRT
  version: an older toolset would build with clang 21, and a newer one may want
  a newer clang still.
- **Rust's toolchain** is `rust-overlay`'s `rust-bin`: rust-lang's own binaries,
  wrapped, with `targets = [ "x86_64-pc-windows-msvc" ]` adding the Windows
  standard library beside the host's. nixpkgs' `rustc` cannot do this job — it
  is rebuilt from source, ships only the host std, and rejects the official
  Windows std outright (`E0514: found crate core compiled by an incompatible
  version of rustc`) — and it also lags the release: the tree has 1.99.1 while
  stable is 1.100.0. Both packages use the same toolchain, so the daemon and
  the UI are always built by one compiler.
- **bindgen.** The bindings are generated on Linux, for MSVC headers, so
  `BINDGEN_EXTRA_CLANG_ARGS` carries `--target=x86_64-pc-windows-msvc` and the
  SDK include directories as `-isystem` (not clang-cl's `/imsvc`, which the GNU
  driver libclang runs in would not parse).
- **pkg-config.** Nothing sets `PKG_CONFIG_PATH` by hand: shaderc is a
  `buildInputs` entry on libplacebo, and nixpkgs' pkg-config wrapper is what puts
  its `lib/pkgconfig` on the path, so the meson cross file's `pkg-config` finds
  it. The one hand-written file is shaderc's `.pc`, because `shaderc_combined`
  is not the target upstream's own file describes — and `libplacebo-sys` reaches
  the same library through `system-deps` rather than ad-hoc probing.

## xwin, and why it is pinned here

nixos-unstable currently packages xwin 0.9.0. This flake needs 0.10.0, so
`nix/sources.nix` builds it from the 0.10.0 tag when nixpkgs' copy is a different
version. The SDK and toolset are not "latest in the manifest": manifest v18 is
fetched and hashed, and the selection inside it is pinned to
`--sdk-version=10.0.28000 --crt-version=14.52`, so a manifest update cannot move
the headers or libraries under the build. Bumping either means bumping the
versions in `nix/sources.nix` and re-hashing the download derivation.

The CRT/SDK stay under Microsoft's licence (`meta.license` is unfree), which is
why `flake.nix` narrows `allowUnfreePredicate` to that one package. Building the
Windows payload is agreeing to those terms.

## Iterating against the flake

`nix build` starts every build in a fresh sandbox, so nothing in a `target/`
directory survives between two derivations. The layer split above is what makes
that bearable: editing a crate rebuilds that crate and its dependents, not the
~500 dependency crates, and the layers are ordinary store paths, so they are
shared by every worktree and by CI through the Nix cache. There is no compiler
cache, no sandbox mount and no impure flag involved.

What invalidates what, in practice:

| edit | rebuilds |
|---|---|
| `crates/ui/src/**` | `siphon-windows-ui` (and the installer) |
| `crates/daemon/src/**` | `siphon-windows-daemon` |
| `crates/shared/src/**` | both binaries, not the dependency layer |
| `Cargo.lock`, `Cargo.toml` | the dependency layer, then both binaries |
| a note, a workflow, `README.md` | the installer only |

For editing code, use the dev shell: it has the same toolchain, CRT/SDK and
libplacebo, and `cargo build` is an ordinary incremental build against
`./target`. The shipped profile knobs (`CARGO_PROFILE_RELEASE_LTO=fat`,
`..._CODEGEN_UNITS=1`) live in `nix/windows.nix`, not in `msvc.env`, so the shell
uses the quick profile and cross artefacts land under
`target/x86_64-pc-windows-msvc/`:

```sh
nix develop .#windows -c cargo build --release -p siphon-ui
nix develop            -c cargo run -p siphon-ui     # native UI
```

A persistent `CARGO_TARGET_DIR` shared by builds would be the obvious
alternative to the layer split and it is a trap: cargo's freshness check
compares mtimes, Nix stamps every source file with 1970, and artefacts from an
earlier build are newer than that, so cargo concludes nothing changed and the
derivation installs a binary from the previous tree. (The content-hash fix,
`-Zchecksum-freshness`, is nightly-only.) Splitting at the dependency boundary
sidesteps the question entirely: each derivation starts from an empty target dir.

## Checks

`checks.<system>.linux-clippy` and `checks.<system>.windows-clippy` are
`craneLib.cargoClippy` on the dependency layer, so the lint runs in the same
environment as the build and reuses its artifacts. CI builds exactly those two;
locally the same command works:

```sh
nix build .#checks.windows-clippy
nix build .#checks.linux-clippy
```

`nix flake check --no-build` evaluates every output; the real `nix flake check`
now builds both lint checks as well, which is a full workspace compile per
target.

## CI caching

The dependency layers are packages of their own — `.#windows-deps`,
`.#linux-deps` — so CI can cache exactly them and nothing that changes per
commit. `release.yml` is two jobs:

1. `deps` builds `.#windows-deps` under `nix-deps-<version>-hash(flake.lock,
   Cargo.lock, nix/**)`. That entry holds the layer plus everything it took to
   build it: the MSVC CRT/SDK, the rust toolchain, the vendored crate sources
   and every compiled dependency. It is rebuilt only when a dependency or a pin
   moves.
2. `windows` restores the same key with `save: false` and builds
   `.#windows-installer`. The two binaries and the installer change on every
   commit, so they are never cached; a run pays two crate compiles and a link.

Both entries are capped at `gc-max-store-size-linux: 8G`: GitHub's cache budget
is 10 GB per repository with LRU eviction, so one entry must not be able to take
the lot. Nixpkgs paths are substitutable and are re-fetched rather than kept,
which is why the entry stays under the cap.

There is deliberately no external binary cache. The trade-off: the cache lives
inside Actions, so a worktree or a fresh checkout still compiles the dependency
layer from scratch — the layers only help the runner. The tree is also clean under `nix fmt` (nixfmt), `statix check`,
`deadnix`, and `nixpkgs-hammering` for both packages.

## The installer

`packages.windows-installer` is packaging *inside* the derivation: it stages the
payload's `bin/` with the licence texts from the tree plus libplacebo's from its
source, runs `makensis`, and zips the payload and the PDBs out of `symbols/`.
`nsis` and `zip` are the only tools it needs — no `cargo`, no `jq`, because the
version comes from `Cargo.toml` through the flake rather than from
`cargo metadata`.

All three artifacts are byte-identical across rebuilds, verified with
`nix build --rebuild .#windows-installer`. NSIS records the staged files'
timestamps and `zip` stores them per entry, so the staging tree and the symbol
copies are pinned to `SOURCE_DATE_EPOCH` before packing, and both zips are
written with `-X`. Without that the setup differed in three bytes and the zip
differed in the entry headers.

Signing stays outside Nix: it needs the PKCS#12 secret, and the artefacts come
out unsigned. `osslsigncode` on `result/*-setup.exe` is the post-build step; the
workflow does not do it today, and it is the reason the payload's own exes are
unsigned too — sign them before `makensis` would mean a second packaging pass.

### Why the Windows binaries needed one more step

`nix build --rebuild .#windows` used to fail on exactly **8 bytes per
executable**, at `RSDS+4`: the CodeView PDB GUID. The cause was not codegen --
`.text`, `.pdata`, headers and the libplacebo DLL are byte-identical -- but
rustc's link step:

1. rustc links a temporary `symbols.o` that lives in a per-invocation directory,
   `/tmp/rustcXXXXXX/symbols.o`.
2. lld records that path in the PDB it writes.
3. lld sets the PDB GUID to `xxh3_64` of the PDB bytes, and stamps the same GUID
   into the image's `RSDS` record.

So the random directory name reached the executable through the PDB hash. The
first fix tried was `-C link-arg=/DEBUG:NONE` (no PDB, no GUID), which is
reproducible but throws away the only thing that can symbolise a minidump from a
release build.

Instead the PDB is kept. `nix/normalise-pe-debug.py` runs after the link, per
binary:

- takes the PDB path from the executable's own `RSDS` record rather than
  assuming it (rustc names it after the crate, so `siphon-daemon.exe` has
  `siphon_daemon.pdb`),
- rewrites `/tmp/rustcXXXXXX` in the PDB to `/tmp/rustcs1ph0n` (same width, so
  nothing shifts; it fails the build if the recorded name stops being `rustc`
  plus exactly six characters, rather than silently leaving a moving GUID),
- recomputes the digest the way lld does (over the file with the PDB stream's
  signature and GUID zeroed),
- writes it back into the PDB header **and** the executable's `RSDS` record.

Both files are then byte-identical across builds and still match each other, so
a Windows `.dmp` resolves its symbols. The PDBs ship in `result/symbols/`, out of
`result/bin/`, so the installer payload does not carry them, and the installer
derivation zips them into `siphon-<version>-x64-symbols.zip` beside the setup. `llvm-pdbutil dump
-summary` and `llvm-readobj --coff-debug-directory` are the structural check that
the two GUIDs agree.

## Not wired up

- The Linux UI needs a Vulkan driver at run time; the dev shell puts
  `vulkan-loader` and `libplacebo` on `LD_LIBRARY_PATH`, but a build machine
  without a GPU can still produce the binaries.
