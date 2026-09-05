# Shade V1 product requirements

**Status:** release candidate  
**Target:** Apple Silicon, macOS and APFS

## Product outcome

Shade gives each host session (a chat, a task, an agent run) an isolated, durable and space-efficient coding workspace. It reuses immutable Git bases and dependency layers through APFS copy-on-write cloning while preserving every agent change across checkpoints, forks, synchronization, recovery and release.

V1 succeeds when 20 concurrent chats can work independently, recover durable operations after client or daemon failure, and finish without lost changes or leaked mutable resources. Common interactions remain compact enough for agent loops: machine JSON only, resumable events and common responses no larger than 512 bytes.

## Public product

Shade ships one Rust binary. The same binary is the machine CLI and the per-user LaunchAgent daemon. A Rust SDK used by the CLI, a TypeScript SDK for host orchestrators, the official operating Skill and the 20-chat harness are part of V1.

The engine has exactly three public seams:

```text
execute(intent, actor, idempotency_key)
query(query)
events(after_cursor)
```

The normative wire shape, outcomes and selector rules are in [Protocol v1](PROTOCOL.md). The CLI exposes `open`, `context`, `heartbeat`, `checkpoint`, `fork`, `sync`, `restore`, `deps refresh`, `publish`, `resolve`, `release` and `events`; administration adds `warm`, `review resolve`, `doctor`, `gc` and `install`.

## Required behavior

- `open` registers or imports a repository, performs a strict fetch, resolves an opaque Git object ID, prepares an immutable base and every detected dependency ecosystem, then returns a live session, workspace, cwd, environment and compact context.
- A workspace has an immutable `base_sha`. Checkpoints preserve HEAD, the real index and working state, including deletions, untracked files, symlinks and executable modes.
- Fork reproduces the complete parent state under independent Git metadata and a new session lease.
- Sync, restore, dependency refresh and reviewed secret merge produce successors. Their predecessor remains available until the caller adopts the returned cwd and environment.
- Publish is squash-only. Local and remote branch movement use compare-and-swap; remote push is opt-in and lease-protected. Integration conflicts produce a resolution workspace.
- Mutating any workspace can never mutate its base, dependency layer or sibling. Production clone failure returns `COW_UNAVAILABLE`.
- The daemon journals operations before side effects, commits completion with its event, reconciles interrupted work at startup and fences stale leases.
- GC rechecks every preservation gate immediately before deletion and operates only on daemon-owned resources.

[Architecture](ARCHITECTURE.md) defines ownership and ordering. [Security](SECURITY.md) defines repository, dependency and secret boundaries.

## Dependency readiness

`open` returns a cwd only after all detected providers are ready. Shade uses exact host toolchains and never installs them.

| Provider | V1 readiness |
|---|---|
| Bun, pnpm, npm | Exactly one supported lock, frozen scriptless fill, offline replay and immutable COW `node_modules` forest. |
| Python/uv | `pyproject.toml` plus `uv.lock`, hashed wheels only, no project/source installation, relocatable immutable COW `.venv`. |
| Cargo | `Cargo.lock`, locked fetch and frozen offline verification; no compilation or project code execution. |
| Go | Byte-stable module/workspace files, module download, verification and network-disabled replay; no build or mutation command. |

Fingerprints include semantic inputs, workspace graph, host tool identity, OS/ABI and policy. They exclude credentials and absolute paths. Build promotion is single-flight, staged, validated and atomic.

## Secret workflow

Shade never imports, anchors, publishes or pushes trees containing paths whose basename begins with `.env`; fetch, checkpoint and publish fail closed before Shade moves a ref. It stores a private `0600` baseline and exposes only file paths, key names and merge classifications. Merge, keep and discard require an explicit review decision; merge writes a successor. Because V1 deliberately combines raw Git access, linked worktrees and one macOS login UID, its clean filter is a porcelain guardrail rather than isolation from malicious same-UID plumbing; [Security](SECURITY.md) defines that boundary precisely.

## Scope boundary

V1 supports no other operating system, processor, filesystem or distributed handoff. It rejects submodules, Git LFS, custom filters and tracked secret files. It provides no human presentation mode, package-runtime installer, VFS, Homebrew package, signing or notarization.

## Acceptance

[The V1 release gate](RELEASE.md) is the only definition of releasable. Automated checks, crash and GC matrices, malicious fixtures, the twenty-session harness, output budgets and recorded APFS performance/space evidence must all pass; a missing artifact blocks release.
