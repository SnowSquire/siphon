# Windows installer

The shipped artefact is one per-user NSIS installer, built on Linux:

```
scripts/package-win.sh          # build, stage, makensis
dist/siphon-<version>-x64-setup.exe
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
  (`taskkill /F`) before replacing files. The daemon's waiter then opens
  `siphon.exe`, which starts a daemon of its own; the daemon being replaced is
  the one that launched the waiter.
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
  DLL must be built with a shader compiler (`shaderc` or `glslang`):
  `scripts/build-libplacebo-win.sh` disables both because it only links, and a
  libplacebo without one cannot compile its shaders, so the window fails at
  render. Build the release prefix with shaderc (vcpkg or MSYS2) or cross-build
  shaderc too.
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
- **CRT.** The exes and DLLs use the dynamic CRT by default. `xwin` ships import
  libraries, not `vcruntime140.dll`/`msvcp140.dll`, so either bundle those from
  the VC++ redistributable or build the whole payload with a static CRT
  (`+crt-static` and `-Db_vscrt=static_from_buildtype`). Pick one and keep it
  consistent across binaries.
- **Signing.** `SIPHON_SIGN_PFX`/`SIPHON_SIGN_PASS` make the script sign both
  exes before packing and the setup after it (`osslsigncode`). Unsigned releases
  work, but SmartScreen warns.

## Version

The tag is `vX.Y.Z` and the setup name carries the crate version. NSIS's
`VIProductVersion` takes four numeric fields, so the script rejects a version
with a prerelease suffix. Nothing was installed from the old MSI, so there is no
upgrade path to preserve.
