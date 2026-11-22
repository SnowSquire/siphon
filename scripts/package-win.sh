#!/usr/bin/env bash
# Stages the Windows release payload and builds the per-user installer.
#
#   scripts/package-win.sh [--no-build]
#
# Builds `siphon.exe` and `siphon-daemon.exe` for x86_64-pc-windows-msvc through
# scripts/cargo-win.sh, collects the libplacebo DLL the UI links, stages the
# licence texts beside them and runs makensis (Fedora: `sudo dnf install
# mingw32-nsis`). Output:
#
#   dist/siphon-<version>-x64-setup.exe
#   dist/siphon-<version>-x64-portable.zip
#
# The setup is what the updater downloads: the release asset must keep the
# `-setup.exe` suffix, and its name carries the tag's version.
#
# Optional environment:
#   SIPHON_TARGET_DIR      release directory to harvest (default: the
#                          x86_64-pc-windows-msvc release dir)
#   SIPHON_FFMPEG_BIN      directory with the LGPL FFmpeg DLLs to bundle
#   SIPHON_FFMPEG_LICENSE  licence text that ships with them (default:
#                          the LICENSE.txt next to SIPHON_FFMPEG_BIN)
#   SIPHON_SIGN_PFX        PKCS#12 used to Authenticode-sign all binaries
#   SIPHON_SIGN_PASS       its password
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target="${SIPHON_TARGET_DIR:-$root/target/x86_64-pc-windows-msvc/release}"
stage="$root/dist/win64"
out_dir="$root/dist"

build=1
if [[ "${1:-}" == "--no-build" ]]; then
  build=0
fi

for tool in makensis jq zip; do
  command -v "$tool" >/dev/null || {
    echo "missing $tool (Fedora: sudo dnf install mingw32-nsis jq zip)" >&2
    exit 1
  }
done

version="$(cargo metadata --no-deps --format-version 1 --manifest-path "$root/Cargo.toml" \
  | jq -r '.packages[] | select(.name == "siphon-ui") | .version')"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "installer versions are numeric only, got '$version'" >&2
  exit 1
fi

if [[ $build -eq 1 ]]; then
  "$root/scripts/cargo-win.sh" build --release --locked -p siphon-ui -p daemon
fi

rm -rf "$stage"
mkdir -p "$stage"

for exe in siphon.exe siphon-daemon.exe; do
  if [[ ! -f "$target/$exe" ]]; then
    echo "missing $target/$exe; run scripts/package-win.sh without --no-build" >&2
    exit 1
  fi
  cp "$target/$exe" "$stage/$exe"
done

# libplacebo-sys copies the DLL into the profile directory for a dynamic build.
shopt -s nullglob
placebo=("$target"/libplacebo-*.dll)
shopt -u nullglob
if [[ ${#placebo[@]} -eq 0 ]]; then
  echo "no libplacebo DLL in $target; build the Windows prefix first" >&2
  exit 1
fi
cp "${placebo[@]}" "$stage/"

# FFmpeg is optional until the UI turns on libplacebo's `libav` feature. When it
# is on, the LGPL DLLs and the licence text that makes shipping them compliant
# go in together, so neither can be forgotten.
if [[ -n "${SIPHON_FFMPEG_BIN:-}" ]]; then
  shopt -s nullglob
  av=(
    "$SIPHON_FFMPEG_BIN"/avcodec-*.dll
    "$SIPHON_FFMPEG_BIN"/avformat-*.dll
    "$SIPHON_FFMPEG_BIN"/avutil-*.dll
    "$SIPHON_FFMPEG_BIN"/swresample-*.dll
    "$SIPHON_FFMPEG_BIN"/swscale-*.dll
  )
  shopt -u nullglob
  if [[ ${#av[@]} -eq 0 ]]; then
    echo "SIPHON_FFMPEG_BIN has no av*.dll" >&2
    exit 1
  fi
  cp "${av[@]}" "$stage/"
  license="${SIPHON_FFMPEG_LICENSE:-$(dirname "$SIPHON_FFMPEG_BIN")/LICENSE.txt}"
  if [[ ! -f "$license" ]]; then
    echo "FFmpeg licence not found at $license; the LGPL build must ship with it" >&2
    exit 1
  fi
  cp "$license" "$stage/LICENSE-FFmpeg.txt"
fi

cp "$root/LICENSE-MIT" "$root/LICENSE-APACHE" "$root/README.md" "$stage/"

# Both shipped faces are OFL and the licence text has to travel with them.
cp "$root/crates/ui/fonts/Archivo-OFL.txt" "$stage/LICENSE-Archivo-OFL.txt"
cp "$root/crates/ui/fonts/NotoColorEmoji-OFL.txt" "$stage/LICENSE-NotoColorEmoji-OFL.txt"

# The meson install does not carry libplacebo's licence; take it from the
# checkout when the prefix was built from source.
if [[ -n "${LIBPLACEBO_SRC:-}" && -f "$LIBPLACEBO_SRC/LICENSE" ]]; then
  cp "$LIBPLACEBO_SRC/LICENSE" "$stage/LICENSE-libplacebo.txt"
fi

# Signing happens before packing so the exes inside the setup carry it too.
sign() {
  osslsigncode sign -pkcs12 "$SIPHON_SIGN_PFX" -pass "${SIPHON_SIGN_PASS:-}" \
    -h sha256 -t http://timestamp.digicert.com -in "$1" -out "$1.signed"
  mv "$1.signed" "$1"
}
if [[ -n "${SIPHON_SIGN_PFX:-}" ]]; then
  command -v osslsigncode >/dev/null || {
    echo "SIPHON_SIGN_PFX is set but osslsigncode is missing" >&2
    exit 1
  }
  sign "$stage/siphon.exe"
  sign "$stage/siphon-daemon.exe"
fi

mkdir -p "$out_dir"
setup="$out_dir/siphon-$version-x64-setup.exe"
makensis -V2 -DVERSION="$version" -DPAYLOAD="$stage" \
  -DVCREDIST="$root/installer/vcredist.ps1" -DOUTFILE="$setup" \
  "$root/installer/siphon.nsi"

if [[ -n "${SIPHON_SIGN_PFX:-}" ]]; then
  sign "$setup"
fi

# The portable copy is the same payload zipped: there is no single exe to ship
# any more. The name deliberately does not end in `-setup.exe`, so the updater
# never mistakes it for an installer.
portable="$out_dir/siphon-$version-x64-portable.zip"
rm -f "$portable"
(cd "$stage" && zip -qr "$portable" .)

size=$(stat -c %s "$setup")
echo "built $setup ($((size / 1024 / 1024)) MiB)"
echo "built $portable ($(stat -c %s "$portable") bytes)"
if (( size > 64 * 1024 * 1024 )); then
  echo "warning: exceeds MAX_UPDATE (64 MiB); raise it in constants.rs or trim the payload" >&2
fi
