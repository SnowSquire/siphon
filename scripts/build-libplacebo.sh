#!/usr/bin/env bash
# Builds and installs the libplacebo that crates/libplacebo-sys links against.
#
#   LIBPLACEBO_SRC=... LIBPLACEBO_PREFIX=... scripts/build-libplacebo.sh
#
# libplacebo generates its shaders as GLSL text per pipeline and compiles them at
# runtime, so the build needs a GLSL to SPIR-V compiler: shaderc, which the
# Vulkan SDK ships as a static library, is upstream's preferred one. Meson finds
# it through pkg-config, so this writes a shaderc.pc for the SDK's copy.
#
# Everything else is off on purpose:
#
#   opengl, d3d11   only the Vulkan backend is used
#   glslang         shaderc replaces it
#   lcms            ICC profiles; the display is described by DXGI, not a profile
#   dovi, libdovi   Dolby Vision reshaping; the sources are HDR10
#   xxhash          only accelerates libplacebo's internal hashing
#
# Runs in git-bash (MSYS) on Windows, which is where cygpath and the MSVC-layout
# paths come from. It assumes there is no Visual Studio: the CRT and the Windows
# SDK come from an xwin splat and clang-cl is the compiler. INCLUDE must not
# contain the Vulkan SDK: meson drops a pkg-config include directory that is
# already in the environment, which silently loses shaderc/shaderc.h.
#
# Every path is taken from the environment when it is already set, so nothing
# here is specific to one machine:
#
#   LIBPLACEBO_SRC     libplacebo checkout            required, no default
#   LIBPLACEBO_PREFIX  install prefix                 ~/.local/libplacebo-shared
#   VULKAN_SDK         Vulkan SDK root                $SCOOP/apps/vulkan/current
#   SCOOP              Scoop root                     ~/scoop
#   XWIN_SYS_ROOT      xwin splat (CRT + Win SDK)     ~/.xwin-cache/splat
#   LIBPLACEBO_VENV    venv holding meson + ninja     $TEMP/libplacebo-venv
set -euo pipefail

# `set -e` is not inherited by a command substitution, so `x=$(a; b)` succeeds
# when only `a` fails. This closes that hole; bash 4.4+.
shopt -s inherit_errexit || {
	echo "$0: needs bash 4.4+ (shopt inherit_errexit)" >&2
	exit 1
}

command -v cygpath >/dev/null || {
	echo "build-libplacebo: needs git-bash/MSYS on Windows (no cygpath)" >&2
	exit 1
}

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(dirname "$here")

for tool in meson ninja clang-cl llvm-lib llvm-strip pkg-config; do
	command -v "$tool" >/dev/null || {
		echo "build-libplacebo: $tool is not on PATH" >&2
		exit 1
	}
done

# TODO: fix this script so it does not need a hand-placed libplacebo checkout.
#
# It used to default to "$root/../balesh/libplacebo", a path an agent guessed
# with no explanation and nothing guarantees. Rather than silently building from
# a directory that probably is not there, stop and say so: decide where
# libplacebo comes from (a pinned clone, a submodule, a tarball) and replace
# this block, or pass LIBPLACEBO_SRC on the command line.
if [ -z "${LIBPLACEBO_SRC:-}" ]; then
	echo "build-libplacebo: LIBPLACEBO_SRC is not set, and the old default was a guess." >&2
	echo "  It used to fall back to $root/../balesh/libplacebo. Nothing guarantees" >&2
	echo "  that layout. Fix this script (fetch or pin libplacebo) and delete this" >&2
	echo "  block, or run with LIBPLACEBO_SRC=<libplacebo checkout>." >&2
	exit 1
fi
lp=$LIBPLACEBO_SRC

scoop=${SCOOP:-$HOME/scoop}
vulkan=${VULKAN_SDK:-$scoop/apps/vulkan/current}
splat=${XWIN_SYS_ROOT:-$HOME/.xwin-cache/splat}
prefix=${LIBPLACEBO_PREFIX:-$HOME/.local/libplacebo-shared}
venv=${LIBPLACEBO_VENV:-${TEMP:-${TMPDIR:-/tmp}}/libplacebo-venv}
SCRATCH=$(mktemp -d)
trap 'rm -rf "$SCRATCH"' EXIT

# Highest version wins; the directory names are versions, so sort -V not ls.
msvc=$(ls "$splat/VC/Tools/MSVC" 2>/dev/null | sort -V | tail -1 || true)
sdk=$(ls "$splat/Windows Kits/10/Include" 2>/dev/null | sort -V | tail -1 || true)

echo "libplacebo: $lp"
echo "prefix:     $prefix"
echo "vulkan sdk: $vulkan"
echo "xwin splat: $splat (MSVC ${msvc:-?}, Windows SDK ${sdk:-?})"
echo "venv:       $venv"

[ -f "$lp/meson.build" ] || {
	echo "build-libplacebo: no libplacebo checkout at $lp (LIBPLACEBO_SRC must point at one)" >&2
	exit 1
}
[ -d "$vulkan/Lib" ] || {
	echo "build-libplacebo: no Vulkan SDK at $vulkan (set VULKAN_SDK)" >&2
	exit 1
}
[ -n "$msvc" ] && [ -n "$sdk" ] || {
	echo "build-libplacebo: no xwin splat at $splat (set XWIN_SYS_ROOT)" >&2
	exit 1
}
[ -f "$venv/Scripts/activate" ] || {
	echo "build-libplacebo: no venv at $venv (set LIBPLACEBO_VENV; create it with" >&2
	echo "  python -m venv <path> && <path>/Scripts/pip install meson ninja)" >&2
	exit 1
}

source "$venv/Scripts/activate"

cat > "$SCRATCH/shaderc.pc" <<EOF
prefix=$(cygpath -m "$vulkan")
includedir=\${prefix}/Include
libdir=\${prefix}/Lib

Name: shaderc
Description: The shaderc library
Version: 2025.1
Cflags: -I\${includedir}
Libs: -L\${libdir} -lshaderc_combined
EOF
# Prepend, so anything else the environment brought still resolves.
export PKG_CONFIG_PATH="$(cygpath -m "$SCRATCH")${PKG_CONFIG_PATH:+;$PKG_CONFIG_PATH}"
# Windows pkgconf rewrites a .pc's prefix from the file's own location otherwise.
export PKG_CONFIG_DONT_DEFINE_PREFIX=1

splat_m=$(cygpath -m "$splat")
export INCLUDE="$splat_m/VC/Tools/MSVC/$msvc/include;$splat_m/Windows Kits/10/Include/$sdk/ucrt;$splat_m/Windows Kits/10/Include/$sdk/um;$splat_m/Windows Kits/10/Include/$sdk/shared"
export LIB="$splat_m/VC/Tools/MSVC/$msvc/lib/x86_64;$splat_m/Windows Kits/10/Lib/$sdk/ucrt/x86_64;$splat_m/Windows Kits/10/Lib/$sdk/um/x86_64"
export PATH="$(dirname "$(command -v meson)"):$(cygpath -m "$vulkan")/Bin:$PATH"

cat > "$SCRATCH/native.ini" <<EOF
[binaries]
c = 'clang-cl'
cpp = 'clang-cl'
ar = 'llvm-lib'
strip = 'llvm-strip'
pkg-config = '$(cygpath -m "$(command -v pkg-config)")'
EOF

cd "$lp"
meson setup --wipe build-clang-shared \
	--native-file "$SCRATCH/native.ini" \
	--buildtype=release \
	--default-library=shared \
	--prefix="$prefix" \
	-Dvulkan=enabled -Dopengl=disabled -Dd3d11=disabled \
	-Dshaderc=enabled -Dglslang=disabled \
	-Ddovi=disabled -Dlibdovi=disabled -Dlcms=disabled -Dxxhash=disabled \
	-Ddemos=false -Dtests=false -Dbench=false -Dfuzz=false \
	-Dvulkan-registry="$lp/3rdparty/Vulkan-Headers/registry/vk.xml" \
	-Dvulkan-sdk="$vulkan"
ninja -C build-clang-shared -j "${BUILD_JOBS:-10}"
meson install -C build-clang-shared
