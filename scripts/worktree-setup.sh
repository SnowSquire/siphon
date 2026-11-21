#!/usr/bin/env bash
# Populate a fresh worktree with the media and dependencies git cannot give it.
#
# This is NOT wired into OpenCode. v2's worktree config has exactly one field,
# `worktree.directory`; there is no setup-script hook, and nothing exports
# OPENCODE_WORKTREE_BASE/PATH. Run it yourself right after `worktree_create`:
#
#   OPENCODE_WORKTREE_BASE=<main checkout> \
#   OPENCODE_WORKTREE_PATH=<new worktree>  \
#   bash <new worktree>/scripts/worktree-setup.sh
#
# `captures/` (the natemek capture plus the mock's built fixtures at
# captures/servermock/) is several GB. On Linux it is cloned with
# `--reflink=auto`: on btrfs/xfs that allocates no blocks, the two copies share
# extents copy-on-write, and a worktree that writes to a segment cannot touch
# the main checkout's copy. On a filesystem without reflink `cp` falls back to a
# real copy (slow but safe). On Windows (and anything else) it hardlinks with
# `cp -al`, which shares the inode instead: the files are read-only test data,
# so nothing may write to a segment in place.
set -euo pipefail

# `set -e` is not inherited by a command substitution, so `x=$(a; b)` succeeds
# when only `a` fails. This closes that hole; bash 4.4+.
shopt -s inherit_errexit || {
	echo "$0: needs bash 4.4+ (shopt inherit_errexit)" >&2
	exit 1
}

base=${OPENCODE_WORKTREE_BASE:?run from the worktree startup step}
new=${OPENCODE_WORKTREE_PATH:?run from the worktree startup step}

for path in captures; do
	src="$base/$path"
	dst="$new/$path"

	[ -d "$src" ] || continue
	[ -e "$dst" ] && continue

	mkdir -p "$(dirname "$dst")"
	case "$(uname -s)" in
		Linux) cp -a --reflink=auto "$src" "$dst" ;;
		*)
			cp -al "$src" "$dst" 2>/dev/null || {
				rm -rf "$dst"
				cp -a "$src" "$dst"
			}
			;;
	esac
done

pnpm -C "$new/servermock" install
