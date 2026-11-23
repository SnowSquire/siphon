# Windows installer

The shipped artefact is one per-user NSIS installer, built on Linux:

```
scripts/package-win.sh          # build, stage, makensis, zip
dist/siphon-<version>-x64-setup.exe
dist/siphon-<version>-x64-portable.zip
```

`scripts/package-win.sh --no-build` reuses the current
`target/x86_64-pc-windows-msvc/release` instead of building it.

## Why NSIS and not an MSI

The updater expected an MSI, but nothing had been installed yet and WiX is
Windows-only to build (`WIX0000`) even as a .NET tool, so packaging would have
meant a Windows machine or Wine. NSIS's `makensis` runs natively on Linux
(Fedora: `mingw32-nsis`) and produces the same shape of install: per-user, an
uninstaller, an Apps & features entry, LZMA compression, a silent mode. The
updater's hand-off moved with it.

`msitools`/`wixl` is the other Linux-native route, but it implements a WiX-3
subset without extensions and without ICE validation.

## The contract with the daemon

- **Release asset**: the name must end `-setup.exe`
  (`siphon-<version>-x64-setup.exe`); that suffix is what `update::parse_release`
  selects. A portable `.exe` or a `.zip` in the same release is ignored, but the
  first `-setup.exe` wins, so publish one architecture per release.
- **Silent install**: `siphon-<version>-x64-setup.exe /S`, per-user, no
  elevation (the installer is `RequestExecutionLevel user`).
- **Install marker**: `HKCU\Software\Ozeniken\Siphon\InstallDir`, written by the
  installer and read by `update::install_mode`. Without it the copy counts as
  portable and updates only open the release page.
- **Processes**: the installer closes `siphon.exe` and `siphon-daemon.exe`
  (`taskkill /F`) before replacing files. The updater runs it as
  `setup.exe /S /RESTART`; `/RESTART` makes the section start
  `$INSTDIR\siphon.exe` once the files are down, and that window starts a
  daemon of its own. A bare `/S` stays headless, so a manual silent install
  does not pop the app. No PowerShell waiter sits between the daemon and the
  setup: the NSIS script is ours, so the installer does the relaunch an MSI
  could not, and `install_and_relaunch` is one `spawn`.
- **Size**: `MAX_UPDATE` in `constants.rs` caps the downloaded installer at
  64 MiB. The packaging script warns when the setup crosses it.
- **Trust**: the installer is fetched over TLS from the GitHub release and run
  without an extra check; Authenticode signing is the layer that makes that
  visible to SmartScreen, not something the updater verifies.

## Layout

```
%LOCALAPPDATA%\Programs\Siphon\
  siphon.exe
  siphon-daemon.exe
  libplacebo-<api>.dll
  [av*.dll]                    only when libplacebo-sys/libav is on
  LICENSE-MIT  LICENSE-APACHE
  LICENSE-Archivo-OFL.txt  LICENSE-NotoColorEmoji-OFL.txt
  [LICENSE-FFmpeg.txt  LICENSE-libplacebo.txt]
  README.md
  uninstall.exe
```

The UI starts the daemon from its own directory, so the two exes cannot be
separated. Config, logs and recordings stay in
`%LOCALAPPDATA%\com.iken.siphon`; uninstall removes the install tree, the
shortcuts and the two registry keys, and leaves that data alone.

## Payload pieces

- **libplacebo.** `libplacebo-sys` copies `libplacebo-<api>.dll` next to the UI
  for a dynamic build; the packager harvests it from the profile directory. The
  DLL must be built with a shader compiler: `scripts/build-shaderc-win.sh`
  cross-builds shaderc for MSVC (static `shaderc_combined`, so no extra DLL) and
  `SHADERC_PREFIX=... scripts/build-libplacebo-win.sh` links it in. Without it
  libplacebo has no GLSL compiler, links, and then fails at the first frame.
  `scripts/build-libplacebo-win.sh` alone still builds the link-only prefix the
  cross-check uses.
- **FFmpeg.** Nothing links it until the UI enables libplacebo's `libav`
  feature. Then `SIPHON_FFMPEG_BIN` names the DLL directory; the script copies
  the `av*` DLLs and refuses to continue without the matching `LICENSE.txt`.
  Use BtbN's `lgpl-shared` build or configure with `--disable-gpl
  --disable-nonfree --disable-version3 --enable-shared --disable-static`.
  `nvdec`, `nvenc`, `cuvid` and `cuda` are **not** on FFmpeg's nonfree list
  (`nv-codec-headers` is MIT and the driver blob is loaded at run time, not
  distributed); the nonfree list is `cuda_nvcc`, `cuda_sdk`, `libnpp`,
  `libfdk_aac` and `openssl`, and the GPL list is x264/x265 and friends.
  `--enable-d3d12va` is the Windows D3D12 decode acceleration and is LGPL.
- **CRT.** The payload uses the dynamic CRT. The UCRT (`ucrtbase.dll` and the
  `api-ms-win-crt-*` forwarders) ships with Windows 10/11 and is never bundled;
  the VCRuntime and C++ runtime (`vcruntime140.dll`, `vcruntime140_1.dll`,
  `msvcp140.dll`) come from the VC++ 2015-2022 x64 redistributable. The
  installer reads the redistributable's own `Runtimes\x64` registry key and,
  when it is absent, downloads `https://aka.ms/vs/17/release/vc_redist.x64.exe`,
  verifies its Authenticode signature and runs it elevated — one UAC prompt,
  while the install itself stays per-user. The portable zip has no installer,
  so the runtime is a plain prerequisite there. Nothing is bundled either way.
- **Signing.** `SIPHON_SIGN_PFX`/`SIPHON_SIGN_PASS` make the script sign both
  exes before packing and the setup after it (`osslsigncode`). Unsigned releases
  work, but SmartScreen warns.

## Portable copy

There is no single exe to hand out any more, so the portable artefact is the
same staged payload zipped as `siphon-<version>-x64-portable.zip`: extract it
anywhere and run `siphon.exe`. It carries no install marker, so `install_mode`
reports `Portable` and the updater only opens the release page. The name does
not end in `-setup.exe`, so `parse_release` never mistakes it for an installer.

## Release

`.github/workflows/release.yml` runs on a `vX.Y.Z` tag and refuses a tag that
does not equal the workspace version. It builds everything on `ubuntu-26.04`:

1. xwin splats the MSVC CRT/SDK; clang-cl, llvm-lib and lld-link do the work.
   Ubuntu 26.04 ships clang 21 and clang-cl (24.04's clang 18 is too old for
   the current MSVC STL and does not package clang-cl).
2. `scripts/build-shaderc-win.sh` produces the static shaderc prefix.
3. `scripts/build-libplacebo-win.sh` links it into the release libplacebo.
4. `scripts/package-win.sh` builds the exes, stages the payload, and emits the
   setup and the portable zip. The step sets the shipped profile knobs
   (`CARGO_PROFILE_RELEASE_LTO=fat`, `..._CODEGEN_UNITS=1`); the workspace
   profile is the quick local one.

The xwin download, the shaderc/libplacebo prefixes and the Rust target dir are
cached; the prefixes are keyed by the shaderc tag and the pinned libplacebo
commit, so a dependency bump rebuilds them. `LIBPLACEBO_REV` and
`SHADERC_VERSION` at the top of the workflow are the pins to bump. The image is
pinned to `ubuntu-26.04` rather than `ubuntu-latest`: the compiler version is
load-bearing (the MSVC STL needs clang 19+), and GitHub's `latest` label
migrates between images on its own schedule. GitHub adds
the source archives to the release itself. The setup is uploaded as a draft, so
the notes and visibility stay a deliberate step.

### Running it locally

`act` can run the job on this box. Point it at a copy of the repo (the checkout
step must not touch the working tree), give the copy a tag, and feed it a push
event:

```
git update-ref refs/tags/v0.5.0 HEAD
act push -e tag-event.json -j windows \
  -P ubuntu-26.04=ghcr.io/catthehacker/ubuntu:act-26.04
```

The publish step is gated on `!env.ACT`, so a local run stops after packaging.
Two local traps: act and the image's git reject the
`extensions.relativeWorktrees` a colocated jj repo writes, so remove that
extension from the copy's `.git/config`; and rootless podman's `docker exec`
fails with an `oom_score_adj` permission error, so run against the rootful
socket (`sudo systemctl enable --now podman.socket`, `DOCKER_HOST=
unix:///run/podman/podman.sock`).

## Version

The tag is `vX.Y.Z` and the setup name carries the crate version. NSIS's
`VIProductVersion` takes four numeric fields, so the script rejects a version
with a prerelease suffix. Nothing was installed from the old MSI, so there is no
upgrade path to preserve.

## Reproducible packaging

`nix build .#windows-installer` runs this script against the Nix payload, and
`nix build --rebuild .#windows-installer` produces byte-identical artifacts.
Two details make that hold: NSIS records the staged files' timestamps, and a zip
stores each entry's, so the staging tree is pinned to `SOURCE_DATE_EPOCH` before
`makensis` (1980-01-01 when unset, as in the workflow), and the portable zip is
written with `zip -X`.
