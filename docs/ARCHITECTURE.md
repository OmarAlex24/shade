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

Checkout policy inventories all paths first, then inspects small blobs and complete `.gitattributes` contents in batches of at most 1,024 through `git cat-file --batch`. The process runner feeds stdin while draining stdout/stderr so large inventories cannot deadlock on pipe backpressure. Batch responses must match the inventory's object ID, type and byte length before content is inspected; all history and retention checks remain mandatory.

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

GC checks current secrets before deleting an eligible directory. A newly discovered change creates a durable review with its key-only preview in the outbox and keeps the workspace. A completed decision permits cleanup only while the current bytes match its immutable private snapshot. Later edits create a new review; stale pending decisions return a superseding review. `keep` leaves a retained workspace outside GC eligibility. Review snapshots remain outside SQLite/Git and are removed with their owning workspace.

Path and content detection share one local scanner. The CLI also serves Git's persistent clean/smudge protocol, withholding a file until its complete contents pass policy. Safe requests pass byte-for-byte; rejected requests return no content. Git history and index retention are validated independently of that porcelain guardrail. Private working files are copied after checkpoint restoration, so a tracked file can retain its safe index version while its detected secret contents follow the successor outside Git. Baseline files are stored separately from metadata. JSON/TOML merges operate on keys; ambiguous or opaque conflicts are checked before successor allocation.

## Failure model

- Operations are idempotent per actor/key and reject key reuse for a different intent.
- WAL + `synchronous=FULL` protects control-plane transitions.
- Private retention refs independently anchor checkpoint HEAD, real index tree and working tree.
- Startup reconciliation expires leases, cancels their pending handoffs, closes interrupted non-terminal operations and removes only known staging artifacts. An interrupted successor adoption is closed with `OPERATION_INTERRUPTED`, but its original actor/key may atomically reclaim the same operation ID and retry the pending handoff.
- Reconciliation also removes locked registrations at the exact `.shade-register-*/worktree` staging shape. If `.git` was already moved but repair did not finish, the empty staging directory is removed before asking Git to remove its registration. Arbitrary nested paths and symlinked stages are ineligible. Temporary Git indexes live beside managed repositories and use the same owned staging cleanup.
- GC checks the lease, operation, review, checkpoint, unmerged-index and detached-commit gates again immediately before deletion.
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

Reconciliation cases use two actual process kills. The first interrupts an
operation to create the incomplete state; the second interrupts startup while
recovering that same pool. No SQLite state is fabricated. The final restart must
preserve the database inode and live workspaces, finish claimed cleanup, and
produce no duplicate recovery events. Before the final restart, transaction cases
also inspect whether the interrupted operation/event pair rolled back or committed,
and publish recovery checks that its single handoff is already durable. One expiry
case uses the isolated harness's one-second lease and actual elapsed time; normal
runtime defaults remain unchanged.
