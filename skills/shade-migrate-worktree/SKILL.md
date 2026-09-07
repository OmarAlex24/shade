---
name: shade-migrate-worktree
description: Move a git worktree into a Shade workspace and reclaim its disk. Use when converting worktrees to Shade sessions.
---

# Worktree to Shade

`$WT` worktree, `$REPO` repo, `$BR` branch.

1. `cd $WT && git add -A && git commit -m wip`; `git status --porcelain` must be empty. If the user refuses a commit, `git stash -u` into `$REPO` and stop.
2. No tracked `.env*`: `git ls-files '.env*' '**/.env*'` must be empty. Keep them in the secret manager or outside the repo; copy into the new cwd. Gitignored `node_modules`, `target`, `dist` do not move; Shade rebuilds deps from the lock as shared layers.
3. JS repos: exactly one of `bun.lock`, `pnpm-lock.yaml`, `package-lock.json`, `npm-shrinkwrap.json`. Regenerate `bun.lockb` as `bun.lock`; delete extras, commit.
4. `shade open $REPO --base $BR --session <id>`; export its `SHADE_*` env, cd to its `cwd`. Without a claude/codex/cursor/kimi/zumith ancestor, add `--owner-pid $$` or run `shade heartbeat`.
5. `shade context`: `head_sha` is the old HEAD, `dependencies.state` `ready`.
6. `git -C $REPO worktree remove $WT` (`--force` if ignored files remain), then `git -C $REPO worktree prune`.
7. Stay in the Shade cwd, or `shade sleep` if parked; `shade wake --session <id>` returns it.
8. Rollback: branch survives; `git -C $REPO worktree add $WT $BR`.
