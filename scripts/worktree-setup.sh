#!/usr/bin/env bash
# OpenCode "Worktree startup script" for this repo:
#
#   bash "$OPENCODE_WORKTREE_PATH/scripts/worktree-setup.sh"
#
# Captured media (the natemek capture plus the mock's built fixtures at
# captures/servermock/) is several GB. Copying it into every worktree is what
# .worktreeinclude used to do and it is far too slow, so hardlink it from the
# main checkout instead: same inode, no extra disk, and `cp -al` is a few
# milliseconds. The files are read-only test data; nothing writes to a segment
# in place.
#
# `cp -al` only works within one filesystem. If a worktree lives on another
# drive it falls back to a real copy.
set -euo pipefail

base=${OPENCODE_WORKTREE_BASE:?run from the OpenCode worktree startup hook}
new=${OPENCODE_WORKTREE_PATH:?run from the OpenCode worktree startup hook}

for path in captures; do
	src="$base/$path"
	dst="$new/$path"

	[ -d "$src" ] || continue
	[ -e "$dst" ] && continue

	mkdir -p "$(dirname "$dst")"
	cp -al "$src" "$dst" 2>/dev/null || {
		rm -rf "$dst"
		cp -a "$src" "$dst"
	}
done

pnpm -C "$new/servermock" install
