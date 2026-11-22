#!/usr/bin/env bash
# Runs cargo against x86_64-pc-windows-msvc with the libplacebo prefix this box
# builds for that target. Cargo has no per-target `env` table, and exporting
# LIBPLACEBO_PREFIX globally would send native builds at a Windows library, so
# the UI's Windows cross-check goes through this wrapper:
#
#   scripts/cargo-win.sh clippy -p siphon-ui --all-targets
#   scripts/cargo-win.sh build --release -p siphon-ui
#
# The prefix is built by scripts/build-libplacebo-win.sh. Plain
# `cargo check -p daemon --target x86_64-pc-windows-msvc` needs none of this:
# the daemon does not link libplacebo.

set -euo pipefail

root="${LIBS_ROOT:-$HOME/Projects/Code/Libs}"
export LIBPLACEBO_PREFIX="${LIBPLACEBO_PREFIX:-$root/libplacebo/win-prefix}"
export VULKAN_SDK="${VULKAN_SDK:-$root/vulkan}"

if [[ ! -d "$LIBPLACEBO_PREFIX" ]]; then
    echo "no libplacebo prefix at $LIBPLACEBO_PREFIX; run scripts/build-libplacebo-win.sh" >&2
    exit 1
fi

for arg in "$@"; do
    case "$arg" in
        --target | --target=*)
            exec cargo "$@"
            ;;
    esac
done
exec cargo "$@" --target x86_64-pc-windows-msvc
