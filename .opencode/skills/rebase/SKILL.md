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

`<repo>` below is the main checkout, `~/code/siphon`. Its `@-` (the newest
described commit of the local line) is the **only** base this skill rebases
onto.

## 1. Check both sides

```bash
cd <worktree>
jj status                        # jj snapshots automatically; @ always holds the work
jj log -r '::@' --no-graph -T 'change_id.short() ++ " " ++ commit_id.short() ++ " " ++ description.first_line() ++ "\n"' | head
```

**A subagent normally leaves `@` empty** (this repo's convention), but if it does
not, those are its uncommitted changes and section 2 carries them into the main
checkout's working copy. The described work is in `@-`, `@--`, … — `jj edit @`
would rebase nothing. Find the **first described commit** of the worktree's
line: that is the source for the rebase.

Then look at the main checkout:

```bash
cd <repo>   # ~/code/siphon
jj log -r '@|@-' --no-graph -T 'change_id.short() ++ " " ++ commit_id.short() ++ " " ++ description.first_line() ++ "\n"'
```

`@` is the working copy; `@-` is the last described commit here and the rebase
destination.

## 2. Move the worktree's commits on top of yours

Rebase onto this checkout's **`@-`**: the newest described commit of your local
line. Do **not** use the `main` bookmark (it can lag the local line, and it is a
publishing pointer, not a base) and do not use `@` (the working copy: the
worktree line would land on top of it, making your working copy an ancestor of
the chain, so it could no longer be moved to the new tip and would be left
undescribed in the middle).

```bash
cd <repo>                                        # ~/code/siphon
jj rebase -s <first-worktree-commit> -d '@-'
# The worktree's @ is its working copy: whatever was uncommitted there is in
# it. Carry that into your own working copy; the squash drops it once empty.
jj diff -r <worktree-@> --stat                   # empty output: nothing to carry
jj squash --from <worktree-@> --into '@'         # removes the emptied source
jj abandon <worktree-@>                          # only if it printed "Nothing changed."
jj rebase -s '@' -d <last-worktree-commit>       # your @ becomes the new tip
```

The last step is what keeps the rule: only `@` is undescribed, and it is the
tip — now carrying the worktree's uncommitted changes, if any, as this
checkout's working-copy changes (`jj status` shows them, `jj diff` is that work).
When the squash has something to apply it removes the emptied `<worktree-@>`;
when it prints `Nothing changed.` the commit was empty, so abandon it yourself.
If the main `@` had changes of its own, the squash merges both (resolve a
conflict as usual). If the worktree's line has an undescribed commit *below* its
tip, describe it (`jj describe -r <change>`) before rebasing, or abandon it if it
is empty.

The rebase gives the worktree commits new commit IDs but keeps their **change
IDs**, so `jj show <change>` and `jj squash --from <change>` still work
afterwards. Verify: `jj chain` shows your commits, then the worktree's, with `@`
on top. `@` is the only undescribed commit and holds whatever the worktree's
working copy had. `jj status` should show exactly those changes and nothing
else; a conflict (from the squash or the rebase) is the one thing to resolve
with `jj diff --git` before going on.

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

## 6. Move main

`@` is the working-copy commit — empty, or carrying the worktree's uncommitted
changes — and the rebased tip is `@-`. Moving the `main` bookmark is the
separate final pointer update; it is never the rebase base.

```bash
cd <repo>
jj bookmark set main -r '@-'
```

The `@` commit is not in the pushed range, so it may stay undescribed. Every
commit below it must be described. If the remote moved, `jj git fetch`,
reconcile the bookmark, and push again.

## 7. Verify

```bash
jj status               # @ is the tip, @- is the rebased commit; @ may have changes
# Nothing below @ is undescribed: no output is the pass.
jj log -r '((fork_point(@ | trunk())::@) ~ @) & description(exact:"")' \
  --no-graph -T 'change_id.shortest(8) ++ "\n"'
jj bookmark list main   # main and main@origin at the rebased tip
jj workspace list       # only "default (current)" (plus unrelated worktrees)
git worktree list       # only this repo (plus unrelated worktrees)
ls <worktree-parent>    # the directory is gone
```

## Pitfalls

- **Empty `@` is the norm, not an exception.** Rebasing *from* the first
  described commit is the operation; `jj edit @` is a no-op.
- **Only `@` may be undescribed, and only at the tip.** An empty, undescribed
  commit below `@` is a defect: `jj describe -r <change>` it, or abandon it if
  it is empty. Do not reason that it "will be squashed later" — it is exactly
  the state this skill must never leave behind.
- **The worktree's `@` is not just an empty tip.** jj snapshots automatically,
  so it holds whatever was uncommitted there. Squash it into your `@` before
  abandoning it, or the uncommitted work dies with the worktree. An empty
  `jj diff -r <worktree-@>` is the only reason to drop it outright.
- **Never rebase onto `main` or onto `@`.** The base is `@-`
  (`~/code/siphon`'s newest described commit). `main` can lag the local line,
  and `@` would become an ancestor of the chain, so the working copy could not
  be moved to the new tip afterwards.
- **Move the previous `@`, do not rebuild it.** `jj rebase -s '@' -d
  <last-worktree-commit>` puts the working copy — with the worktree's changes
  now in it — back at the tip. Replacing it with `jj edit`/`jj new` can leave the
  old `@` behind as a second head.
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
