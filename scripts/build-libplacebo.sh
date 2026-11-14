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
# This machine has no Visual Studio, so the CRT and Windows SDK come from the
# xwin splat and clang-cl is the compiler. `INCLUDE` there must not contain the
# Vulkan SDK: meson drops a pkg-config include directory that is already in the
# environment, which silently loses shaderc/shaderc.h.
set -euo pipefail

LP=${LIBPLACEBO_SRC:-C:/Users/alex/Projects/Code/twitch-siphon/balesh/libplacebo}
PREFIX=${LIBPLACEBO_PREFIX:-C:/Users/alex/.local/libplacebo-shared}
VULKAN=${VULKAN_SDK:-C:/Users/alex/scoop/apps/vulkan/current}
SPLAT=${XWIN_SPLAT:-C:/Users/alex/.xwin-cache/splat}
VENV=${LIBPLACEBO_VENV:-${TEMP:-/tmp}/libplacebo-venv}
SCRATCH=$(mktemp -d)
trap 'rm -rf "$SCRATCH"' EXIT

[ -f "$LP/meson.build" ] || { echo "no libplacebo checkout at $LP" >&2; exit 1; }
[ -d "$VULKAN/Lib" ] || { echo "no Vulkan SDK at $VULKAN" >&2; exit 1; }

source "$VENV/Scripts/activate"

cat > "$SCRATCH/shaderc.pc" <<EOF
prefix=$(cygpath -m "$VULKAN")
includedir=\${prefix}/Include
libdir=\${prefix}/Lib

Name: shaderc
Description: The shaderc library
Version: 2025.1
Cflags: -I\${includedir}
Libs: -L\${libdir} -lshaderc_combined
EOF
export PKG_CONFIG_PATH=$(cygpath -m "$SCRATCH")
# Windows pkgconf rewrites a .pc's prefix from the file's own location otherwise.
export PKG_CONFIG_DONT_DEFINE_PREFIX=1

msvc=$(ls "$SPLAT/VC/Tools/MSVC" | tail -1)
sdk=$(ls "$SPLAT/Windows Kits/10/Include" | tail -1)
splat=$(cygpath -m "$SPLAT")
export INCLUDE="$splat/VC/Tools/MSVC/$msvc/include;$splat/Windows Kits/10/Include/$sdk/ucrt;$splat/Windows Kits/10/Include/$sdk/um;$splat/Windows Kits/10/Include/$sdk/shared"
export LIB="$splat/VC/Tools/MSVC/$msvc/lib/x86_64;$splat/Windows Kits/10/Lib/$sdk/ucrt/x86_64;$splat/Windows Kits/10/Lib/$sdk/um/x86_64"
export PATH="$(dirname "$(command -v meson)"):$(cygpath -m "$VULKAN")/Bin:$PATH"

cat > "$SCRATCH/native.ini" <<EOF
[binaries]
c = 'clang-cl'
cpp = 'clang-cl'
ar = 'llvm-lib'
strip = 'llvm-strip'
pkg-config = '$(cygpath -m "$(command -v pkg-config)")'
EOF

cd "$LP"
meson setup --wipe build-clang-shared \
	--native-file "$SCRATCH/native.ini" \
	--buildtype=release \
	--default-library=shared \
	--prefix="$PREFIX" \
	-Dvulkan=enabled -Dopengl=disabled -Dd3d11=disabled \
	-Dshaderc=enabled -Dglslang=disabled \
	-Ddovi=disabled -Dlibdovi=disabled -Dlcms=disabled -Dxxhash=disabled \
	-Ddemos=false -Dtests=false -Dbench=false -Dfuzz=false \
	-Dvulkan-registry="$LP/3rdparty/Vulkan-Headers/registry/vk.xml" \
	-Dvulkan-sdk="$VULKAN"
ninja -C build-clang-shared -j "${BUILD_JOBS:-10}"
meson install -C build-clang-shared
