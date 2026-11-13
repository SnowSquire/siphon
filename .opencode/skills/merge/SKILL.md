---
name: Merge worktree back
description: Merge an OpenCode worktree's work into this jj repo, move the session into the main checkout, and delete the worktree.
---

# Merge a worktree back into the main repo

A worktree at `~/.local/share/opencode/worktree/<project>/<name>` is a **jj
workspace on this same repo** (shared `.jj`, one commit store, detached git
HEAD), not a clone. So there is nothing to fetch, push or `git merge`: the
worktree's commits are already in this repo's history and are visible from
here with `jj log` / `jj edit`. Only the *workspace pointers* differ.

`.` is the default workspace (this directory). Every live worktree shows up as
an extra entry in `jj workspace list` (named `opencode-<uuid>`), and as a second
path in `git worktree list`.

## 1. Check the worktree

```bash
cd <worktree>
jj status          # jj snapshots automatically: a dirty tree is already IN @
jj log -r '@|@-' --no-graph -T 'change_id.short() ++ " " ++ description.first_line() ++ "\n"'
```

Nothing needs stashing or committing first — `@` always holds the work. Note the
**change id** of `@`; it is what you move next.

## 2. Merge: check the worktree's commit out here

```bash
cd <repo>          # this directory
jj log -r 'all()' --no-graph -T 'change_id.short() ++ " " ++ commit_id.short() ++ " " ++ description.first_line() ++ "\n"' | head
jj edit <change-id>
jj status          # the worktree's files are now the working copy here
```

`jj edit` is the whole merge. It rewrites files into this workspace's working
copy and leaves the worktree untouched. jj 0.47 allows a commit to be checked
out in two workspaces at once (`edit` succeeds even while the worktree still
exists); if a future jj refuses, run `jj workspace forget <workspace>` from the
worktree's `jj workspace list` entry first, then retry.

If the worktree's history has diverged from this repo's `@` (e.g. both sides
have commits), rebase instead of checking out the old tip:

```bash
jj rebase -s <change-id> -d '@'     # put the worktree work on top of this @
```

Then `jj edit` the resulting commit.

## 3. Finalize

```bash
jj commit -m "<type>: <what the worktree did>"    # leaves an empty @ on top
jj log -r '@|@-' --no-graph
```

Keep the work as its own commit rather than squashing it into an unreviewed
`wip:` parent — the maintainer can squash later with `jj squash --from <id>`.

## 4. Move the session, then delete the worktree

Both are tools, not shell commands, and the order matters — the session cannot
outlive the directory it is sitting in.

```
tools.opencode.session_move({ directory: "<repo>" })
tools.opencode.worktree_remove({ directory: "<worktree>" })
```

`worktree_remove` **will refuse with "it has modified or untracked files"** even
though `jj status` is clean. Its check is git's, and the colocated git HEAD /
index lags behind jj: every file jj ever touched looks modified. Confirm there is
no unique content, then force it:

```bash
diff -r --exclude=target --exclude=.git --exclude=.jj --exclude=node_modules \
     --exclude=segments --exclude=captures --exclude=manifest \
     "<worktree>" "<repo>"     # only ignored build artefacts may differ
```

```
tools.opencode.worktree_remove({ directory: "<worktree>", force: true })
```

Do not `rm -rf` the worktree and do not call `git worktree remove`: that leaves a
stale `jj workspace` entry and jj then errors on every command in the repo.

## 5. Verify

```bash
jj workspace list     # only "default (current)"
git worktree list     # only this repo
ls ~/.local/share/opencode/worktree/<project>/    # worktree dir gone
jj status             # @ empty, @- is the merged commit
```

## Pitfalls

- **`jj workspace list` names the workspace, not the worktree directory.** The
  entry is `opencode-<uuid>`; match it by commit id, not by name.
- **The two workspaces share one op log.** A `jj commit` / `describe` / `squash`
  here rewrites `@`, and the worktree's `@` (same change id) follows on its next
  jj command — which is why the worktree reads "no changes" after the merge
  instead of showing the diff you just committed.
- **`jj status` in the worktree after the merge is not evidence of anything**;
  check `jj log` in the main repo instead.
- **`git` is a trap here**: detached HEAD, stale index, and it reports the
  merged files as modified. Use `jj diff --git` / `jj log`, and only force the
  worktree removal after the `diff -r` check above.
- The worktree startup hook hardlinks `captures/` and `servermock/segments/`
  from this repo (`scripts/worktree-setup.sh`); deleting the worktree never
  touches the originals.
