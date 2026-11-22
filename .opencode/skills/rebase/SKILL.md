---
name: Rebase worktree back
description: Rebase an OpenCode worktree's commits into this jj repo, rescue media the worktree created, and delete the worktree.
---

# Rebase a worktree back into the main repo

A worktree created by `worktree_create` is a **jj workspace on this same repo**
(shared `.jj`, one commit store, detached git HEAD), not a clone. There is
nothing to fetch or push: its commits are already in this repo's history and
visible with `jj log`. Only the *workspace pointers* differ, and the two lines
almost always fork from a common base — so this is a **rebase**.

Use the absolute directory `worktree_create` returned — it is
`worktree_under/<repo-parent>` in some setups and
`~/.local/share/opencode/worktree/<project>/<name>` in others; do not assume.
`.` (this directory) is the default workspace. Each live worktree is an extra
`opencode-<uuid>` entry in `jj workspace list` and a second path in
`git worktree list`.

## 1. Check both sides

```bash
cd <worktree>
jj status                        # jj snapshots automatically; @ always holds the work
jj log -r '::@' --no-graph -T 'change_id.short() ++ " " ++ commit_id.short() ++ " " ++ description.first_line() ++ "\n"' | head
```

**A subagent normally leaves `@` empty** (this repo's convention), so the work
is in `@-`, `@--`, … — `jj edit @` would rebase nothing. Find the **first
described commit** of the worktree's line: that is the source for the rebase.

Then look at the main checkout:

```bash
cd <repo>
jj log -r 'ancestors(@, 10)' --no-graph -T 'change_id.short() ++ " " ++ description.first_line() ++ "\n"'
```

## 2. Move the worktree's commits on top of yours

```bash
cd <repo>                                  # @ here is your empty working copy
jj rebase -s <first-worktree-commit> -d '@'
jj abandon <worktree-empty-@>              # the empty tip, if it came along
jj edit <last-worktree-commit>             # @ = the rebased tip
jj new                                     # empty @ on top
jj abandon <the-empty-commit-that-was-your-@>   # leaves no empty commit mid-history
```

The rebase gives the worktree commits new commit IDs but keeps their **change
IDs**, so `jj show <change>` and `jj squash --from <change>` still work
afterwards. Verify: `jj log -r '::@'` (or `jj chain`) shows your commits, then
the worktree's, with `@` empty and undescribed on top. `jj status` must be clean
— if it is not, the rebase dropped something or left a conflict; inspect with
`jj diff --git` before going on.

Do **not** squash the worktree's commits into one, and do not squash them into
an undescribed parent: keep them as their own described commits so they can be
reviewed or squashed later with `jj squash --from <id>`.

## 3. Rescue anything the worktree *created* that git ignores

The startup step clones `captures/` from this repo *into* the worktree (reflink
on btrfs, hardlink on Windows), so those files already exist here. But anything
the worktree **generated** —
`servermock/segments/`, a fresh `captures/<name>/`, build output — exists only
in that directory and dies with it. Copy it out first, hardlinked:

```bash
[ -e <repo>/captures/servermock ] || cp -al <worktree>/servermock/segments <repo>/captures/servermock
stat -c '%h %n' <repo>/captures/servermock/chunks/000.ts   # link count >= 2 means shared, no extra disk
```

Repeat for every ignored path the task produced. Check with
`git -C <worktree> status --ignored --short | head` if unsure.

## 4. Confirm nothing unique is lost

`diff -r` between the directories is **not** a useful check: the worktree's
working copy is its own (usually older) base, so it differs wherever *your* side
moved on — notes, `todo.md`, files you split or deleted. That is staleness, not
lost work.

Compare the worktree's committed change instead:

```bash
jj diff --from <common-base> --to <last-worktree-commit> --stat   # what it actually changed
```

Then check only those paths exist identically in this checkout (for a
`servermock/**`-only task, `diff -r <worktree>/servermock <repo>/servermock` —
excluding the ignored media — must be empty). A diff in files the worktree never
committed is expected and means nothing.

## 5. Remove the worktree

`worktree_remove` fails even with `force: true` in a jj-colocated repo (its check
is git's, and git's index lags jj). Forget the workspace instead — that removes
both the jj workspace and git's worktree registration:

```bash
jj workspace forget <opencode-uuid>     # prints "Removed Git worktree for ..."
jj workspace list                       # entry gone
git worktree list                       # entry gone
```

Then delete the directory (safe *after* the forget — the warning about `rm -rf`
is about deleting it while the workspace is still registered):

```bash
rm -rf <worktree>
```

If you are sitting in the worktree, `tools.opencode.session_move({ directory:
"<repo>" })` first: the session cannot outlive its directory. Never call
`git worktree remove` — it leaves the jj workspace entry and every later jj
command errors.

## 6. Verify

```bash
jj status               # @ empty, @- is the rebased commit
jj workspace list       # only "default (current)" (plus unrelated worktrees)
git worktree list       # only this repo (plus unrelated worktrees)
ls <worktree-parent>    # the directory is gone
```

## Pitfalls

- **Empty `@` is the norm, not an exception.** Rebasing *from* the first
  described commit is the operation; `jj edit @` is a no-op.
- **`jj workspace list` names the workspace, not the directory.** The entry is
  `opencode-<uuid>`; match it by commit id.
- **The two workspaces share one op log.** A `jj commit`/`describe`/`rebase` here
  rewrites `@`, and the worktree's `@` (same change id) follows on its next jj
  command — which is why the worktree then reads "no changes" instead of showing
  the diff you just committed. `jj status` in the worktree after the rebase is
  not evidence of anything.
- **Ignored media is per-directory.** The clone the setup script made is just a
  copy; files the worktree generated point nowhere else. Rescue those before
  removing it.
- **`git` is a trap**: detached HEAD, stale index, reports files as modified.
  Use `jj diff --git` / `jj log`, and only use git for `git worktree list`.
- **`rm -rf` only after `jj workspace forget`**. Deleting a registered workspace
  leaves a stale entry and jj errors on every command in the repo.
- `scripts/worktree-setup.sh` clones `captures/` from this checkout (reflink on
  btrfs, hardlink on Windows). OpenCode does **not** run it: v2 has no worktree
  setup hook, so it must be run by hand after `worktree_create`. Deleting the
  worktree never touches the originals.
