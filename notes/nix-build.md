# Nix build

`flake.nix` builds both halves the release ships, hermetically: the Linux daemon
and UI from nixpkgs' toolchain and libplacebo, and the Windows `siphon.exe` +
`siphon-daemon.exe` cross-compiled with clang-cl, lld-link and the MSVC CRT/ SDK.
It reuses the same toolchain and the same library options as
`scripts/cargo-win.sh` and `scripts/build-libplacebo-win.sh`; what it adds is
that every input — including the ones those scripts download — has a hash.

```sh
nix build .#linux              # siphon (Linux UI) + siphon-daemon in result/bin
nix build .#windows            # siphon.exe + siphon-daemon.exe + libplacebo-*.dll
nix build .#windows-installer  # the per-user setup and the portable zip
nix build                      # .#linux
nix develop                    # the Linux toolchain, without the -devel packages
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

`nix/windows.nix` mirrors the shell scripts, in the same order:

1. **shaderc**, CMake with the `msvc-cross.cmake` toolchain file, target
   `shaderc_combined`, installed as a static archive plus a `.pc` that names it.
   libplacebo needs a GLSL compiler at run time, and linking it in keeps the
   payload to two exes and one DLL.
2. **libplacebo**, meson with `msvc-cross.ini`, the cross file carrying the
   `/imsvc` SDK includes and the `-libpath` import libraries. The options match
   `scripts/build-libplacebo-win.sh` (Vulkan on, GL/D3D11/glslang off, shaderc
   on, codecs and lcms off), so the DLL the UI links behaves like the shipped
   one.
3. **Rust**, for `x86_64-pc-windows-msvc` with
   `CC_x86_64_pc_windows_msvc=clang-cl`, `lld-link` as the linker and the SDK
   import directories on `RUSTFLAGS`. `libplacebo-sys` is pointed at the prefix
   with `LIBPLACEBO_SOURCE=prefix`, which is also how it finds the DLL to copy
   next to the executable.

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

## Checks

`nix flake check --no-build` evaluates every output; the real `nix flake check`
builds them. The tree is also clean under `nix fmt` (nixfmt), `statix check`,
`deadnix`, and `nixpkgs-hammering` for both packages.

## The installer

`packages.windows-installer` is `scripts/package-win.sh --no-build` with
`SIPHON_TARGET_DIR` pointing at the payload's `bin/`, so the setup ships exactly
what `.#windows` built. It needs `makensis`, `zip`, `jq` and a `cargo` for
`cargo metadata --no-deps` (all in `nativeBuildInputs`).

Both artifacts are byte-identical across rebuilds, verified with
`nix build --rebuild .#windows-installer`. That needed one change to the
script: NSIS records the staged files' timestamps and `zip` stores them per
entry, so `package-win.sh` now pins the staging tree to `SOURCE_DATE_EPOCH`
(1980-01-01 when it is unset, which is what the workflow gets) before
`makensis`, and zips with `-X`. Without that the setup differed in three bytes
and the zip differed in the entry headers.

Signing stays outside Nix: it needs the PKCS#12 secret, so `SIPHON_SIGN_PFX`
remains a property of whoever runs the packaging.

## Not wired up

- The Linux UI needs a Vulkan driver at run time; the dev shell puts
  `vulkan-loader` and `libplacebo` on `LD_LIBRARY_PATH`, but a build machine
  without a GPU can still produce the binaries.
