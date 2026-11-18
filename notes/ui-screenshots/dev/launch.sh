#!/bin/sh
# Dev loop: restart the UI with external CSS/fonts so visual changes need no
# rebuild. The daemon is killed too because the RPC protocol changed in this
# pass; the UI spawns a fresh one on startup.
set -eu
pkill -x siphon 2>/dev/null || true
pkill -x siphon-daemon 2>/dev/null || true
sleep 1
export WAYLAND_DISPLAY=wayland-1
export XDG_RUNTIME_DIR=/run/user/1000
export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
export RUST_LOG=info
export SIPHON_SHEET=/tmp/opencode/siphon-dev/sheet.css
export SIPHON_THEME_CSS=/tmp/opencode/siphon-dev/theme.css
export SIPHON_FONT_UI=/tmp/opencode/fonts/Archivo.ttf
export SIPHON_FONT_BRAND=/tmp/opencode/fonts/chillax-extract/Chillax_Complete/Fonts/TTF/Chillax-Variable.ttf
cd /home/sheo/code/siphon
nohup ./target/debug/siphon > /tmp/opencode/siphon-dev/ui.log 2>&1 &
echo $! > /tmp/opencode/siphon-dev/ui.pid
