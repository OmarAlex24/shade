# Case study: 186 worktrees on one laptop

A real measurement of what linked `git worktree` checkouts cost on a working
machine, and what converting them to Shade gives back. The "before" column is
recorded. The "after" column is filled in once the conversion described in
[MIGRATING.md](MIGRATING.md) has been run.

## Method

Reproduce it with the inventory script in this repository:

```sh
mkdir -p /tmp/wt-inventory
zsh scripts/worktree-inventory.sh /tmp/wt-inventory
```

- **Discovery.** `find` over `$HOME` and `/private/tmp` to a depth of 6,
  pruning `Library`, `.Trash`, `node_modules`, `target` and `.next`, matching
  `.git` *files* — a `.git` file rather than a directory is exactly what marks
  a linked worktree. Each worktree is attributed to its parent repository by
  reading the `gitdir:` line of that file.
- **Sizing.** `du -sk` per worktree for the total; `du -sk` again over
  `target`, `.next`, `dist`, `build`, `out`, `.turbo`, `.zumith-studio`,
  `.zenith-studio` for build output; over `node_modules`, `.venv`, `vendor`,
  `.pnpm-store` for dependencies, plus a bounded search for nested
  `node_modules` in monorepos. Checkout bytes are the remainder,
  `total - build - deps`.
- **Scope.** The scan covered the worktree roots created by Codex, Cursor,
  Conductor and Zenith Studio under `$HOME`, plus scratch trees under
  `/private/tmp`. Repositories without a linked worktree are not counted.
- **Caveat.** `du -sk` reports allocated blocks per tree. A hardlinked or
  cloned package store is counted in every tree that references it, so
  per-tree figures are apparent sizes. The per-row TSV is authoritative over
  any figure quoted in prose here.

Raw rows: `artifacts/worktree-baseline-2026-09-06.tsv` (gitignored, not
included in this repository; regenerate it with the command above).

## Before, measured 2026-09-06

| Category | Size | Share |
| --- | --- | --- |
| Build output | 70.4 GB | 49% |
| Dependencies | 59.4 GB | 42% |
| Checkouts | 12.7 GB | 9% |
| **Total across 186 linked worktrees** | **142.5 GB** | 100% |

Concentration, not spread, is the story:

- One Rust `target/` directory alone held 66.7 GB — 95% of all build output and
  47% of everything measured.
- One repository accounted for 58 of the 186 worktrees, each carrying a
  `node_modules` of roughly 2.2 GB.

## What Shade should reclaim

| Bucket | Mechanism | Expected |
| --- | --- | --- |
| Dependencies | one shared layer per lock fingerprint instead of one install per worktree | most of 59.4 GB |
| Checkouts | copy-on-write clones of one immutable base per commit | most of 12.7 GB |
| Build output | reclaimed only by `shade sleep`; an awake workspace keeps its build directory | 0 GB while awake, up to 70.4 GB parked |
| **Converting only** | | **~60 GB** |
| **Converting and sleeping the parked branches** | | **~130 GB** |

The gap between those two numbers is the whole argument for `shade sleep`:
about half the disk on this machine was build output that no one was going to
look at again, and only sleeping a workspace gives it back.

## After

Rerun the inventory once the conversion is complete and fill this in. Add the
Shade footprint (`~/Library/Application Support/Shade`) as its own row: shared
bases and dependency layers are real bytes, and the honest number is the
difference, not the reclaimed side alone.

| Metric | Before | After | Delta |
| --- | --- | --- | --- |
| Linked worktrees | 186 | TODO | TODO |
| Shade sessions (active) | 0 | TODO | TODO |
| Shade sessions (suspended) | 0 | TODO | TODO |
| Build output | 70.4 GB | TODO | TODO |
| Dependencies | 59.4 GB | TODO | TODO |
| Checkouts | 12.7 GB | TODO | TODO |
| Shade bases + layers | 0 GB | TODO | TODO |
| **Net on disk** | **142.5 GB** | **TODO** | **TODO** |

Also record, because they decide whether this is repeatable:

- TODO — worktrees that could not be converted, and which blocker from
  [MIGRATING.md](MIGRATING.md) stopped each one.
- TODO — median wall time for one conversion, and the slowest one.
- TODO — `shade wake` time for a slept workspace versus a fresh dependency
  install in a new `git worktree`.
