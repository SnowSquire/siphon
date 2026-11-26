# Windows installer

The shipped artifact is one per-user NSIS installer, built on Linux by the
flake:

```
nix build .#windows-installer
result/siphon-<version>-x64-setup.exe
result/siphon-<version>-x64-portable.zip
result/siphon-<version>-x64-symbols.zip
```

`<version>` is `0.5.1`, or `0.5.1-18b8dd2` on a build that is not a release:
`.#windows-installer-sha` takes a `label` holding the short commit, so two
builds of one version can be told apart and the portable zip's inner directory
carries it too. Only a `vX.Y.Z` tag builds the plain name, which is the one
`update::parse_release` expects.

`nix/windows-installer.nix` is the whole packaging step: it stages the payload
`nix/windows.nix` built, adds the license texts, runs `makensis`, and zips the
portable copy and the PDBs. `notes/nix-build.md` covers the toolchain and pins.
There is no packaging script, and nothing is downloaded at build time.

## Why NSIS and not an MSI

The updater expected an MSI, but nothing had been installed yet and WiX is
Windows-only to build (`WIX0000`) even as a .NET tool, so packaging would have
meant a Windows machine or Wine. NSIS's `makensis` runs natively on Linux and
produces the same shape of install: per-user, an uninstaller, an Apps & features
entry, LZMA compression, a silent mode. The updater's hand-off moved with it.

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
- **Autostart**: the installer writes
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\Siphon` as
  `"<installdir>\siphon.exe" --minimized`, and the uninstaller deletes it. So an
  installed copy comes up at sign-in, into the tray, and starts the daemon
  itself; `--minimized` hides the window after its first frame and the tray's
  Open shows it. A portable copy has no Run value and does not autostart.
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
  LICENSE-MIT  LICENSE-APACHE
  LICENSE-Archivo-OFL.txt
  LICENSE-libplacebo.txt  LICENSE-shaderc.txt  LICENSE-glslang.txt
  LICENSE-SPIRV-Tools.txt  LICENSE-SPIRV-Headers.txt
  LICENSE-fast_float.txt  LICENSE-Vulkan-Headers.txt
  [av*.dll]                    only when libplacebo-sys/libav is on
  [LICENSE-FFmpeg.txt]
  README.md
  uninstall.exe
```

The UI starts the daemon from its own directory, so the two exes cannot be
separated. Config, logs and recordings stay in
`%LOCALAPPDATA%\com.iken.siphon`; uninstall removes the install tree, the
shortcuts and the three registry keys (install marker, uninstall entry and the
autostart value), and leaves that data alone.

## Payload pieces

- **The UI's icon.** `siphon.exe` embeds `crates/ui/icons/icon.ico` as an
  `RT_GROUP_ICON` resource at build time (`crates/ui/build.rs`; see
  `notes/riffle.md`). That resource, not anything the running window does, is
  what Explorer, the Start-menu shortcut, `DisplayIcon` and a pinned task-bar
  button show. The installer's own icon is the separate `MUI_ICON` in
  `installer/siphon.nsi`, which covers setup and uninstall.
- **libplacebo.** The UI links libplacebo's archive, because the flake asks
  `nix/msvc.nix` for `libplaceboLink = "static"`. There is no DLL to harvest and
  the payload stays two files; a `shared` cross environment still works —
  `libplacebo-sys` copies `libplacebo-<api>.dll` next to the UI and the packager
  would take it — but the shipped artifact is static. Under
  LGPL-2.1-or-later that is §6, so the payload owes relink material: the sources
  and the pinned flake are published (MIT), and `nix build .#windows` rebuilds
  the executable against a replaced libplacebo. The build must have a shader
  compiler: `nix/msvc.nix` cross-builds shaderc for MSVC (static
  `shaderc_combined`, so no extra DLL) and links it into libplacebo. Without it
  libplacebo has no GLSL compiler, links, and then fails at the first frame.
  Because it is static, glslang, SPIRV-Tools, SPIRV-Headers, fast_float and the
  Vulkan headers are in the same image too; every one of their texts lands in the
  payload beside `LICENSE-libplacebo.txt`, and `README.md` has the component
  table.
- **FFmpeg.** Nothing links it until the UI enables libplacebo's `libav`
  feature, and the Nix payload ships none of it yet. When it does, the packaging
  derivation has to copy the `av*` DLLs from a Nix-built FFmpeg and the license
  text with them.
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
- **Signing.** The artifacts come out unsigned: a Nix build cannot hold the
  PKCS#12 secret, and the workflow has no signing step. `osslsigncode sign
  -pkcs12 ... -in result/*-setup.exe -out ...` is the post-build step, but it
  only covers the setup — signing the installed exes too means signing the
  payload and re-running the packaging derivation, which is why the two steps
  would have to be reconciled before it is wired up. Unsigned releases work,
  but SmartScreen warns.

## Portable copy

There is no single exe to hand out any more, so the portable artifact is the
same staged payload zipped as `siphon-<version>-x64-portable.zip`: extract it
anywhere and run `siphon.exe`. It carries no install marker, so `install_mode`
reports `Portable` and the updater only opens the release page. The name does
not end in `-setup.exe`, so `parse_release` never mistakes it for an installer.

## Symbols

The release PDBs are kept, so every build also emits
`siphon-<version>-x64-symbols.zip` with `siphon.pdb` and `siphon_daemon.pdb`.
They stay out of the installer payload — a user never needs them — and are
published beside it: a minidump is useless without the matching PDBs, and the
GUID in each PDB is what the debugger matches against the dump. The dependency
layer is built with `CARGO_PROFILE_RELEASE_DEBUG=1` and the binaries with `2`,
so every frame resolves to a source line and the workspace's own code also keeps
its locals and types; the executable is unaffected because the CodeView data
lives in the PDB. The packaging derivation takes them
from the payload's `symbols/`, which is where
`nix/windows.nix` puts the normalized copies. rustc names a PDB after its
crate, so the daemon's file is `siphon_daemon.pdb`, not `siphon-daemon.pdb`.

## Release

`.github/workflows/release.yml` runs on a `vX.Y.Z` tag, calls `build.yml`, and
refuses a tag that does not equal the workspace version. Two jobs, both just Nix:

1. `cachix/install-nix-action` puts Nix on the runner; nothing else is
   installed — no compiler, no xwin, no meson.
2. `deps` builds `.#windows-deps` and caches it under a key made of the
   lockfile and the flake pins, so the MSVC CRT/SDK, shaderc, libplacebo, the
   vendored sources and every compiled dependency survive between runs.
3. `windows` restores that entry read-only (`save: false`) and runs
   `nix build .#windows-installer`. The pins live in `nix/sources.nix` and
   `flake.lock`, not in the workflow, and the shipped profile (fat LTO, one
   codegen unit) is the derivation's `env`.
4. `softprops/action-gh-release` publishes the setup, the portable zip and the
   symbols zip as a draft; manual and nightly runs upload the same three as
   workflow artifacts instead, since they never create a tag.

The image is pinned to `ubuntu-26.04` rather than `ubuntu-latest` so the Nix
daemon and the runner's kernel do not move without a commit. GitHub adds the
source archives to the release itself. The setup is uploaded as a draft, so the
notes and visibility stay a deliberate step.

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

`nix build --rebuild .#windows-installer` produces byte-identical artifacts.
Two details make that hold: NSIS records the staged files' timestamps, and a zip
stores each entry's, so the staging tree and the PDBs are pinned to
`SOURCE_DATE_EPOCH` (which the stdenv sets) before packing, and both zips are
written with `zip -X`.
