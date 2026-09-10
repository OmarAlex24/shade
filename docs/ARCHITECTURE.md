# Architecture

Shade has one public seam:

```text
execute(intent, actor, idempotency_key)
query(query)
events(after_cursor)
```

The CLI and both SDKs are projections of that seam. The engine owns sequencing; callers cannot compose filesystem and Git primitives in an unsafe order.

The binary parses its command before creating Tokio. Ordinary CLI calls use a current-thread runtime for socket I/O and deadlines; the daemon uses a multithreaded runtime for concurrent sessions. Help and version responses require no runtime.

```text
Host orchestrator / agent
      │
Rust SDK / TypeScript SDK / machine CLI
      │  versioned NDJSON over a 0600 Unix socket
      ▼
┌──────────────────── shade-engine ────────────────────┐
│ operation journal → lifecycle → transactional outbox │
│          │              │                │            │
│       SQLite        concrete GitStore   events        │
│          │              │                             │
│          ├──────── WorkspaceFilesystem(APFS)          │
│          ├──────── DependencyProvider(*)              │
│          └──────── private secret baselines           │
└───────────────────────────────────────────────────────┘
```

SQLite is the control-plane authority, Git owns content, and the filesystem owns materialized views. They are not collapsed into a single “workspace state” field: repositories, sessions, workspaces, leases, operations, checkpoints, reviews and dependency receipts have distinct tables and transitions.

## Lifecycle ordering

An operation is inserted before side effects. Filesystem content is built in daemon-owned staging, verified, atomically published, registered with Git, and only then marked ready. Completion and its event are committed together. If the client times out, the server task continues; the durable operation is recoverable by ID or cursor.

`open` performs a strict explicit-refspec fetch, resolves an opaque commit ID, prepares one immutable base, COW-clones it, registers a detached locked linked worktree, verifies its index, prepares all detected dependency ecosystems, captures local secret baselines, creates the session lease, and returns cwd/env.

The base that fetch asks for is resolved against the repository being opened before the remote, because that repository is the only place an unpushed branch exists and it is what the `git worktree add` Shade replaces would have read. A commit id, `refs/heads/*` and `refs/tags/*` come from there and are anchored under `refs/shade/bases/`, which is neither `refs/remotes/origin/*` -- that would claim a local branch is on the remote -- nor `refs/heads/*`, which is the namespace a publish compare-and-swaps. An explicit `origin/<branch>` still names the remote and is still fetched from it. Either way the objects arrive through the same quarantine and the same checkout policy: a local path is a Git remote URL like any other.

Checkout policy inventories all paths first, then reads only the blobs a structural decision needs -- complete `.gitattributes` contents and blobs small enough to be an LFS pointer -- in batches of at most 1,024 through `git cat-file --batch`. The process runner feeds stdin while draining stdout/stderr so large inventories cannot deadlock on pipe backpressure. Batch responses must match the inventory's object ID, type and byte length before content is inspected. The base commit's tree is held to the whole policy; reachable ancestors are checked only for gitlinks, LFS and filter declarations, the features Shade cannot materialize under any name, so a walk of a long history costs an inventory rather than a content pass. Index and retention checks remain mandatory.

`WorkspaceFilesystem::clone_immutable_tree` is reserved for verified daemon-owned bases and dependency layers held immutable throughout materialization. Its APFS inventory rejects unsupported entries and crossing filesystem boundaries before selecting the bounded directory clone: at most 25,000 entries and no internal hardlinks. These eligible trees use `fclonefileat` from a pinned source descriptor. Mutable workspaces, larger trees and trees with hardlinks use controlled per-entry cloning; internal hardlinks are recreated only within the destination. Both paths preserve symlinks and portable modes, stage beside the destination, and publish exclusively after syncing. A failed clone returns `COW_UNAVAILABLE` with no byte-copy fallback.

Fork, sync, restore, dependency refresh and reviewed secret merge create new workspaces. No integration path rewrites the predecessor. Successor preparation and its operation outcome commit atomically while the predecessor lease remains live; a partial unique constraint and transactional validation allow only one pending handoff per session. A separate adoption CAS swaps session/lease and its final outcome atomically. Pending handoffs protect both workspaces from GC and are cancelled if the predecessor expires. A squash publish updates a local branch through compare-and-swap; remote push is opt-in and lease-protected.

Fork preserves the copied `node_modules` forests and `.venv` instead of deleting them during Git restoration or rematerializing cached bytes. JavaScript/Python receipts follow that snapshot. Location-dependent Python entrypoints, activation files and links are retargeted to the child while preserving package edits and parent contents. External Cargo/Go caches are revalidated before activation.

Dependency preparation holds a shared artifact lock through workspace receipt persistence. Layer GC acquires its exclusive side before reading protected fingerprints, so it cannot retire a newly promoted layer between promotion and database registration. Preparations remain parallel and share per-fingerprint construction. Deletion first renames an owned layer to a private retirement directory; interruption while removing its payload cannot leave a partial artifact under a valid fingerprint. Startup removes interrupted fill/replay stages, promotions and retirement directories.

Checkpoint and fork use an optimistic content fence in addition to the engine lifecycle lock: Shade captures HEAD, the real index tree and the complete working tree repeatedly, and creates private retention refs only after two consecutive captures agree. A workspace that keeps changing returns `WORKSPACE_NOT_QUIESCENT`; no partial checkpoint refs are published.

Release commits the lease, session and released workspace together. A secret keep/discard decision commits its review, retention state and operation result together, and remains actionable after the lease expires. Secret merges commit the review and successor handoff in the same transaction. When the reviewed child was already adopted by the parent's session, the next handoff starts from that current child; historical lease ownership identifies its predecessor.

A publish conflict commits the resolution workspace state, blocked dependency state,
original branch/push/CAS intent and their events together. Before that commit, the
workspace remains incomplete and startup can remove it. After commit, `resolve`
retains the publication action across restart. This prevents an interrupted publish
from leaving a generic conflict workspace that would only hand off without publishing.

GC checks current secrets before deleting an eligible directory. A newly discovered change creates a durable review with its key-only preview in the outbox and keeps the workspace. A completed decision permits cleanup only while the current bytes match its immutable private snapshot. Later edits create a new review; stale pending decisions return a superseding review. `keep` leaves a retained workspace outside GC eligibility. Review snapshots remain outside SQLite/Git and are removed with their owning workspace. A `failed` workspace is asked a different question, because it has no baseline to be compared against: the capture that would have taken one is the last step of an open and of successor materialization, after the dependency preparation and the checkpoint restore that are what failed. It was also never leased, so nothing but Shade ever wrote in it, and every private file it holds is either content the repository records at those very bytes or a copy of something the predecessor still has -- in its tree, or in the suspension vault, neither of which the copy consumed. GC proves that preservation path by path and collects the tree when it holds; a path that is neither gets the ordinary review and keeps the workspace. `doctor` reports `workspaces_failed` and, separately, `workspaces_failed_awaiting_review`: a pending review takes a workspace out of candidacy until a person answers it, so the second number is the disk no sweep will reclaim on its own.

Path and content detection share one local scanner. One predicate decides which `.env` names are private, and the ingress check, the checkpoint exclusion, the Git attributes, the clean filter, the sleep vault and the secret review all read it, so a template name is admitted or refused identically everywhere. Ingress decides on names and structure; content decides what may newly enter Git. The CLI serves Git's persistent clean/smudge protocol, withholding a file until its complete contents pass policy on the clean side, which is the side that writes blobs; a smudge request returns bytes the object database already holds and passes through. The clean side judges new content only: on a match it asks the running repository what it records for that path -- the index entry first, then HEAD -- and admits bytes whose object ID is identical, which is what keeps checkpoint staging, base verification and a woken successor's index working in a repository whose committed source matches the detector. Safe requests pass byte-for-byte; rejected clean requests return no content. What the admitted base tree already carries is surveyed once with the same windowed scanner and recorded as a `repository.tracked_secret_matches` event holding a count and paths, never a fragment. Git history and index retention are validated independently of that porcelain guardrail. Private working files are copied after checkpoint restoration, so a tracked file can retain its safe index version while its detected secret contents follow the successor outside Git. Baseline files are stored separately from metadata. JSON/TOML merges operate on keys; ambiguous or opaque conflicts are checked before successor allocation.

## Workspace lifecycle

A session and its workspace share one lifecycle with four states. This is the
vocabulary reported by `CompactContext.lifecycle` and the `session` query; on
disk the workspace row reads `ready` while the session is `active`. Only one
transition destroys work.

| State | Tree on disk | Lease | Reached by | Leaves by |
| --- | --- | --- | --- | --- |
| `active` | materialized | live | `open`, `attach`, adoption of a successor | lease expiry, `release` |
| `dormant` | materialized | none | lease expiry (no heartbeat for 120 s) | `attach` / `open` on the same session, `release` |
| `suspended` | reclaimed | none | `sleep`, from `active` or `dormant` | `wake`, `release` |
| `released` | deleted after GC | none | `release` | — |

Dormancy is the ordinary resting state of an agent that stopped talking, not a
fault. A dormant workspace keeps its tree, its checkpoints, its retention refs
and its secret decisions; it is excluded from GC candidacy, and its mutations
fail with `LEASE_EXPIRED` carrying the attach command in `next`. Reattachment is
a single transaction that renews the lease at the next fence, restores the
workspace to `ready` and emits `session.reattached` with `lease.acquired`; it is
idempotent for a session that already holds a live lease. Deletion is always
explicit: `release` is the only path out of a live session into `released`, and
the collector reaches it only after the grace period. The other collectible
state is not a resting place at all -- a workspace whose open or wake failed is
recorded `failed`, and it waits out the same grace.

Suspension is the state that trades the tree for everything else. `sleep` takes
a checkpoint with reason `sleep`, vaults the private files a checkpoint cannot
hold, unregisters the worktree and removes the directory; the record, the
session id, the checkpoints and the retention refs all stay. It refuses a
workspace with unfinished work of its own -- a pending review, another running
operation, a pending handoff or publication, a non-ready checkpoint -- with
`WORKSPACE_NOT_QUIESCENT`.

`wake` is the only way back, and it builds a successor rather than reviving the
record in place: a new workspace id and cwd, cloned from the immutable base,
restored from the sleep checkpoint, refilled from the shared dependency
fingerprint and given back its vaulted files. That is not a compromise; it is
what keeps every crash-recovery path already proven, because a half-built
successor is an ordinary incomplete workspace and the suspension it was built
from is untouched until the final transaction. The handoff machinery cannot be
reused here: adoption requires a live lease on the predecessor, and a suspension
has none, so the predecessor is released and the successor bound in one
transaction that also writes the operation journal.

Suspension has one optional tier under it. `sleep` throws away everything Git
ignores along with the tree, and on a working repository that is most of the
disk it just freed. Point `SHADE_PARK_ROOT` at an absolute path on another
volume and those bytes are copied there instead, into
`<park_root>/<workspace>/<checkpoint>/` as a `manifest.json` beside a `tree/`,
published with one rename so a park directory is either absent or complete.
What is parked is the private gitignored set, minus two things that must never
reach that volume: the dependency layers, which every receipt names and which
`wake` refills from the shared fingerprint anyway, and everything the secret
policy claims, which the sleep vault already holds. That filter is applied
twice, once over the inventory and again for every file as it is copied.
`sleep` parks only when a root is configured, the volume is a writable
directory at that moment, and the tree is at least `SHADE_PARK_MIN_BYTES`
(256 MiB by default), below which a cross-volume copy costs more time than the
boot disk gets back. Every copy is a real byte copy: a park root is a different
volume by definition, `clonefileat` cannot span one, and there is deliberately
no clone path here to fall back to.

`wake` copies the park back into the successor before handing it over, and only
when the manifest names this exact workspace, checkpoint, HEAD, worktree and
layout version. A park that describes anything else, or that is not there at
all, restores nothing and the successor regenerates its own build output; a
path the base now tracks or the vault now owns is skipped for the same reason,
because those are the copies Git and Shade can reproduce. The park is spent by
the wake that used it and is removed with its record, unless
`SHADE_PARK_KEEP_AFTER_WAKE=1` keeps it for a person who wants to look. `gc`
reconciles records against the volume: a park is garbage once it stops
describing a live suspension -- its workspace was released or deleted, or a
later sleep gave it a different suspension checkpoint -- and is then removed
with its record; a record whose directory is gone from a mounted volume is
dropped; a directory no record claims is removed as an orphan. Nothing is ever
removed from a volume that is not mounted, because an unplugged disk is not
evidence that a park is gone, so those are counted as retained and reconsidered
on a later pass.

The tier is a cache and nothing depends on it. A park holds only output a build
can produce again -- the checkpoint holds what Git tracks, the vault holds the
private files -- so the park volume may be wiped, filled or unplugged between
any two commands without changing what `sleep`, `wake`, `release` or GC mean.
Every failure is a sentence rather than an error: no root, no volume, no space,
or a park that describes another tree leaves the operation doing exactly what it
did before the tier existed and says which in `park_reason`.

`release` is the only door to deletion, so a workspace with no tree left must
still be able to walk through it. When the `.git` pointer is gone -- a
suspension, a workspace reconciliation marked `failed`, or a tree something
outside Shade removed -- release skips the status check, the release checkpoint
and the secret review, because none of them can read a tree that is not there,
and goes straight to the release transaction. The trade is deliberate and it is
wider than the case that motivated it: any workspace whose pointer has gone
takes that path, and whatever its working tree held that was never checkpointed
is not recoverable afterwards. `checkpoint_id` comes back `null`, which on its
own does not distinguish this from a clean release with nothing to checkpoint,
so the release also emits `workspace.released_without_tree` naming the reason.

Two sweeps automate the ends of the lifecycle. `SHADE_AUTO_SLEEP_DAYS` sleeps a
workspace that has been dormant that long, and `SHADE_SUSPENDED_RETENTION_DAYS`
releases a suspension that old. Both are off in the engine's own defaults, which
is what a library caller and every test gets, but a daemon nobody attends is the
one case where nothing else will ever reclaim an abandoned tree: `shade install`
therefore writes `SHADE_AUTO_SLEEP_DAYS=3` into the LaunchAgent when the
installing shell named no span of its own, and restates the shell's own value
when it did -- including an empty one, which is how an operator asks for no
sweep at all. Both run through the ordinary verbs, so every gate a manual
`sleep` or `release` applies still applies, and releasing is still not deleting:
GC keeps all of its own gates and its grace period afterwards.

Sessions and workspaces recorded as `orphaned` by an earlier version are
normalized to `dormant` at startup, which is a value change and not a schema
change: neither column carries a CHECK constraint and `user_version` stays at 1.
Suspension adds no schema change either -- `suspended` and `suspending` are two
more values in the same two columns. The parked tier adds one table, `parks`,
created on every open instead of behind a `user_version` bump: an older binary
never writes to a table it does not know about, and a database it wrote gains
the table the first time a newer binary opens it.

## Failure model

- Operations are idempotent per actor/key and reject key reuse for a different intent.
- WAL + `synchronous=FULL` protects control-plane transitions.
- Private retention refs independently anchor checkpoint HEAD, real index tree and working tree.
- Startup reconciliation first finishes or reverts any interrupted suspension, so every later pass sees a workspace that is cleanly `suspended` or cleanly `dormant`. It then normalizes the retired state names, expires leases into dormancy, cancels their pending handoffs, closes interrupted non-terminal operations and removes only known staging artifacts. A workspace that gave its tree up on purpose -- a suspension, or the predecessor a wake released out of one -- is never read as corruption: marking those `failed` would hand a whole suspension to the collector. An expired lease never deletes a tree; it only drops the session to `dormant`. An interrupted successor adoption is closed with `OPERATION_INTERRUPTED`, but its original actor/key may atomically reclaim the same operation ID and retry the pending handoff.
- Reconciliation also removes locked registrations at the exact `.shade-register-*/worktree` staging shape. If `.git` was already moved but repair did not finish, the empty staging directory is removed before asking Git to remove its registration. Arbitrary nested paths and symlinked stages are ineligible. Temporary Git indexes live beside managed repositories and use the same owned staging cleanup.
- GC only ever considers `released`, `failed` and the retired `orphaned` workspaces, so neither a dormant nor a suspended workspace is a candidate at any point. It checks the lease, operation, review, checkpoint, unmerged-index and detached-commit gates again immediately before deletion, and a `failed` workspace additionally has to prove that every private file in its tree is preserved outside it.
- Common replies stay compact; [durable failure diagnostics](DIAGNOSTICS.md) are referenced by `diagnostics_id`. Sanitized detail commits with the failed operation and outbox event; explicit CLI/SDK queries retrieve it. The CLI can also read the existing database while the daemon is stopped.

The deterministic full-copy `CopyFilesystem` exists only behind the `test-support` Cargo feature, which the crate enables for its own integration tests and never for the distribution binary; a release build has no byte-copy `WorkspaceFilesystem` to reach, so `COW_UNAVAILABLE` cannot degrade into silent copying. The Apple Silicon/APFS gate is enforced by `Engine::with_components_and_git`, the single constructor body every other constructor funnels through, so `with_components` and `open` reject an unsupported platform identically with `PLATFORM_UNSUPPORTED`: tests cannot run on a construction path production cannot reach.

The `fault-injection` Cargo feature adds a test-only rendezvous at named phase boundaries. With an explicitly armed isolated directory, a selected boundary writes a marker and stops the daemon with `SIGSTOP`; the test parent sends `SIGKILL` and restarts the same pool. The distribution build compiles these calls to no-ops and does not read fault-related environment variables. The matrix checks real APFS directories, Git registrations/refs, SQLite integrity/foreign keys, predecessor contents, operation replay and final resource disposition.

Incremental-base cases interrupt temporary-index creation, population, refresh,
tree transition, verification and atomic promotion. The fixture changes binary
content, deletes a file, changes a mode and symlink, and replaces a file with a
directory. Recovery must retain the exact previous immutable base and either no
new base or the complete new tree; retry must finish without losing agent edits.
Publish-conflict cases interrupt allocation, integration and both sides of the
resolution transaction, then verify unchanged branches before resolution and a
single-parent squash on both local and explicitly requested remote branches after it.

Resolved-publication cases cover dependency readiness, checkpoint phases, local
and remote CAS, completion, anchor cleanup and handoff. Recovery must finish one
publication, preserve the original workspace, replay its operation idempotently
and remove every temporary anchor.

Lifecycle cases cut a suspension at each of its four commits and a wake at both
of its two. An interrupted sleep must leave a tree that is still registered and
still usable, or a `suspending` workspace that startup finishes from its sleep
checkpoint; either way the workspace ends `suspended` with no tree, no live
lease, and content that comes back on the next wake. An interrupted wake must
leave the suspension it was building from untouched and its half-built successor
collected as the ordinary incomplete workspace it is, and once the binding
transaction commits, its operation must replay without waking a second time.

Reconciliation cases use two actual process kills. The first interrupts an
operation to create the incomplete state; the second interrupts startup while
recovering that same pool. No SQLite state is fabricated. The final restart must
preserve the database inode and live workspaces, finish claimed cleanup, and
produce no duplicate recovery events. Before the final restart, transaction cases
also inspect whether the interrupted operation/event pair rolled back or committed,
and publish recovery checks that its single handoff is already durable. One expiry
case uses the isolated harness's one-second lease and actual elapsed time; normal
runtime defaults remain unchanged.
