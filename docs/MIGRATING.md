# Moving existing worktrees into Shade

If you have run coding agents on a Mac for a while, you have a directory tree
full of linked `git worktree` checkouts: Codex, Cursor, Conductor and Claude
Code all create them, none of them clean up, and every one carries its own
`node_modules`, `target` or `.venv`. This guide converts those worktrees into
Shade sessions one at a time and gives the disk back.

Read [the Shade README](../README.md) first for what a workspace is. The
mechanical, per-worktree procedure lives in the
[`shade-migrate-worktree` skill](../skills/shade-migrate-worktree/SKILL.md);
this document is the plan around it.

## What changes and what does not

Shade replaces the worktree an *agent* would have created for itself. It does
not take over your editor, your shell or your tooling.

| | Before | After |
| --- | --- | --- |
| Checkout bytes | one full copy per worktree | one immutable base per commit, cloned copy-on-write |
| Dependencies | one install per worktree | one shared layer per lock fingerprint |
| Build output | kept forever, per worktree | kept while the workspace is awake, reclaimed by `shade sleep` |
| Ownership | untracked | a session with a lease, checkpoints and an audit trail |
| Cleanup | `git worktree remove` by hand | `shade sleep` to park, `shade release` to end |

Things Shade deliberately leaves alone:

- **Tools that create worktrees themselves keep doing so.** Conductor, Cursor
  and Codex manage their own trees through their own UI. Shade does not
  intercept them. Converting those is a per-tool decision you make by turning
  the feature off in that tool, not something Shade can do for you.
- **`AGENTS.md` only redirects agent-initiated worktrees.** The snippet in
  [AGENTS-SNIPPET.md](AGENTS-SNIPPET.md) tells an agent to call `shade open`
  instead of `git worktree add`. It has no effect on a worktree a human or a
  host application creates.
- **Your main checkout stays a normal Git checkout.** Shade opens workspaces
  *from* it; it never converts or moves it.

## Step 1: inventory

Measure before you delete anything. `scripts/worktree-inventory.sh` walks
`$HOME` and `/private/tmp`, finds every linked worktree by its `.git` file,
and reports total, build and dependency bytes per worktree and per repository.

```sh
mkdir -p /tmp/wt-inventory
zsh scripts/worktree-inventory.sh /tmp/wt-inventory
```

It writes `wt.txt` (the worktree paths it found) and `wt.tsv` (one row of
`total<TAB>build<TAB>deps<TAB>path<TAB>repository`, in KiB), then prints the
totals, the fifteen largest worktrees and the fifteen heaviest repositories.
Keep `wt.tsv`: it is the "before" column of your own numbers, and rerunning
the script afterwards produces the "after" column. See
[CASE-STUDY.md](CASE-STUDY.md) for a worked example.

Sort the rows into three piles:

- **Active** — a branch you are still working on. Convert it (step 2).
- **Parked** — real work, not being touched this week. Convert it, then
  `shade sleep` it immediately. The session, its checkpoints and its private
  files survive; the tree does not occupy disk.
- **Stale** — merged, abandoned, or a scratch tree you forgot. Confirm the
  branch is merged or worthless, then `git worktree remove` and delete the
  branch. Do not convert garbage.

The pile boundaries matter more than the order. Most of the reclaimed space
comes from the parked and stale piles, not from the active one.

## Step 2: convert one worktree

Follow the [`shade-migrate-worktree` skill](../skills/shade-migrate-worktree/SKILL.md).
In outline, for a worktree at `$WT` on branch `$BR` belonging to `$REPO`:

1. Commit the work in progress, including untracked files
   (`git add -A && git commit`). A Shade workspace is materialized from a Git
   commit, so anything uncommitted does not come across. If you do not want a
   commit, `git stash -u` — the stash lives in `$REPO`, and you are done with
   Shade for that tree.
2. Move `.env*` and any other secret out of the tree, into your secret manager
   or a path outside the repository. Shade rejects tracked `.env*`. Untracked
   secrets are simply absent from the new workspace, so copy them into the new
   cwd afterwards.
3. If it is a JavaScript repository, make sure exactly one lockfile is
   committed.
4. `shade open $REPO --base $BR --session <id>` from the main repository, then
   enter the returned `cwd` and export the returned `SHADE_*` environment.
5. `shade context` — check `head_sha` against the old worktree's HEAD and
   `dependencies.state` is `ready`.
6. `git -C $REPO worktree remove $WT`, then `git -C $REPO worktree prune`.

Rollback is always available: nothing in this procedure touches the branch, so
`git -C $REPO worktree add $WT $BR` recreates exactly the layout you had.

## Step 3: batch

Convert in order of size, not of age — the inventory's top-fifteen list is the
work queue. Do one repository at a time so a lockfile problem blocks one
repository instead of the whole afternoon.

- Convert the active branches and keep working in the Shade cwd.
- Convert the parked branches and `shade sleep` each one straight away.
- Delete the stale ones outright; they never become Shade sessions.

## What you should expect to save

- **Checkouts and dependencies: immediately.** The checkout becomes a
  copy-on-write clone of a shared base, and the dependency tree becomes a
  shared layer keyed by lock fingerprint. In the
  [case study](CASE-STUDY.md), one repository held 60 worktrees and 45.2 GB of
  dependencies across the 36 of them that had any installed; as Shade sessions
  that is one layer per lock fingerprint.
- **Build output: only when you sleep.** A checkpoint is taken with
  `git add -A`, so gitignored build output — `target`, `dist`, `.next`,
  `node_modules` — is not preserved and is not restored. That is the point:
  `shade sleep` gives those bytes back, and `shade wake` re-links the shared
  dependency layer rather than reinstalling it. A workspace you keep awake
  keeps its build directory and its bytes.
- **Nothing, if you only convert.** Converting an active workspace and leaving
  it awake trades one full checkout for a clone and a shared layer. That is
  most of the win on dependencies and almost none of it on builds.

## Known blockers

| Blocker | Symptom | Fix |
| --- | --- | --- |
| Two JS lockfiles in one repository | `multiple JavaScript lockfiles are present; keep exactly one` | Keep one of `bun.lock`, `pnpm-lock.yaml`, `package-lock.json`, `npm-shrinkwrap.json`, delete the rest, pin `packageManager` in `package.json`, commit. A binary `bun.lockb` must be regenerated as a text `bun.lock`. |
| No lockfile at all | dependency lock is missing | Commit one. Shade never resolves an unpinned tree, and it never installs a runtime or toolchain for you. |
| Tracked `.env*` | the open is rejected before checkout | `git rm --cached` the file, add it to `.gitignore`, keep the values in your secret manager, and copy them into the new cwd after `shade open`. |
| Uncommitted state in the worktree | work silently missing from the new workspace | Commit it or `git stash -u` before you convert. There is no third option: Shade materializes commits. |
| A plain shell or a CI job | the lease expires after 120 s and the session goes `dormant` | The keepalive only attaches to a recognised agent ancestor (`claude`, `codex`, `cursor`, `cursor-agent`, `kimi`, `zumith`). From anything else, pass `--owner-pid <pid>` or `--owner-name <name>` to `shade open`, or run `shade heartbeat` on a timer. A dormant session is not lost: `shade attach --session <id>` takes a fresh lease. |
| Submodules, Git LFS, custom Git filters | the repository is rejected | Not supported in V1. Those repositories stay on `git worktree`. |
