# Shade

Shade turns a Git repository into many isolated, instantly created coding workspaces for AI agents. Each workspace is an APFS copy-on-write clone of an immutable base plus a shared dependency layer, so 20 agents get 20 full working trees in milliseconds without 20 copies of `node_modules`. Everything an agent does is durable: leases, checkpoints, forks, sync, publish as a squash commit, crash recovery and resumable events all survive a client or daemon restart.

## The problem

`git worktree` gives you isolation, but every worktree reinstalls its dependencies, and nothing tracks which agent owns which tree, what it changed, or what happens when it crashes mid-operation. A container per agent solves isolation and reproducibility but is slow and heavy on a laptop. Shade sits in between: native filesystem speed through APFS cloning, plus an agent-aware lifecycle with leases, checkpoints and recoverable operations.

## Who it is for

Shade is for authors of agent orchestrators and IDE-like hosts that run many coding agents in parallel on a Mac. If you are building the thing that spawns agents, Shade gives each one a workspace and a durable record of what it did.

Platform constraint, stated up front: **Apple Silicon, macOS and APFS only.** There is no Linux, CI, Intel Mac or non-APFS support yet. Unsupported targets fail closed. Production materialization requires a successful APFS clone syscall on the same filesystem; a failure returns `COW_UNAVAILABLE` and there is no byte-copy fallback.

## How it works

- One immutable base per resolved Git commit. Bases are prepared once and reused.
- Workspaces are APFS clones of that base, created with `clonefileat` / `fclonefileat` against pinned directory descriptors, staged beside the destination and atomically published only after they verify.
- Dependency layers are shared across workspaces by fingerprint. Fingerprints cover lock contents, the workspace graph, host tool identity, OS/ABI and the isolation policy; they exclude credentials and absolute paths.
- Dependency installs run in a sandbox. Offline replay runs under macOS `sandbox-exec` with network operations denied for the process tree, and JavaScript lifecycle scripts run only after an explicit per package/version/integrity approval.
- Sessions hold a lease. SDK session handles heartbeat in-process. The CLI starts a detached keepalive only when it can find an *agent* among the ancestors of the process that ran `shade open` — by default one of `claude`, `codex`, `cursor`, `cursor-agent`, `kimi` or `zumith`, overridable with `SHADE_OWNER_PROCESS_NAMES` or named outright with `--owner-pid` / `--owner-name`. A plain shell is not an agent: `open` reports `keepalive.state` as `owner_not_detected` and nothing renews the lease, so run `shade heartbeat` yourself or pass `--owner-pid $$`. The keepalive stops when the process it was bound to exits. A lease expires after 120 seconds without a heartbeat.
- An expired lease makes a workspace **dormant**, not garbage. A dormant workspace keeps its tree, its checkpoints and its secret decisions, is never garbage-collected, and `shade attach --session <id>` brings it back with a fresh lease. Only `shade release` makes a workspace collectible.
- `shade sleep` **suspends** a workspace: it takes a checkpoint, moves the private files a checkpoint cannot hold into a store outside Git, and gives the tree back to the filesystem. The session, its history and its identity all survive. `shade wake --session <id>` rebuilds the content as a successor workspace with a new id and cwd, restoring the dependency layer from its shared fingerprint rather than reinstalling it.
- Checkpoints capture HEAD, the real index tree and the complete working tree, including deletions, untracked files, symlinks and executable modes.
- Sync, restore, dependency refresh and reviewed secret merge produce a **successor** workspace through a two-phase handoff. The predecessor stays live until the caller adopts the new cwd and env. No integration path rewrites or deletes a predecessor.
- Publish is squash-only, with compare-and-swap on the branch and opt-in, lease-protected push. A conflict is a first-class outcome that returns a resolution workspace.
- Secrets are scanned before any tree is imported, checkpointed or published. Detected changes produce a review containing only paths, key names and a classification, never values.
- Operations are journaled in SQLite (WAL, `synchronous=FULL`) before side effects, are idempotent per `(actor, key)`, and are reconciled at daemon startup. Transport is NDJSON over a per-user Unix socket with mode `0600`.

## Quick start

Requirements: Apple Silicon, macOS, APFS, Rust 1.93+, Git, plus whichever package managers the target repository declares. Python repositories need uv >= 0.11.16.

```sh
cargo build --release
./target/release/shade install
```

`shade install` copies the same binary to `~/Library/Application Support/Shade/bin/shade`, writes a private LaunchAgent plist for the `com.shade.daemon` label, and bootstraps it in your GUI launchd domain. The one binary serves both the CLI and the daemon.

Every command prints exactly one minified JSON value on stdout. `shade --help` and `shade --version` print JSON too. `events --follow` prints JSONL. There is no prompt, color, table or human presentation mode.

```zsh
open_key='open:task-42'
opened="$(shade --idempotency-key "$open_key" open /absolute/repo --session task-42)"
test "$(jq -r '.status' <<<"$opened")" = ok
test "$(jq -r '.outcome.state' <<<"$opened")" = completed
export SHADE_SESSION="$(jq -r '.outcome.result.env.SHADE_SESSION' <<<"$opened")"
export SHADE_WORKSPACE="$(jq -r '.outcome.result.env.SHADE_WORKSPACE' <<<"$opened")"
export SHADE_LEASE="$(jq -r '.outcome.result.env.SHADE_LEASE' <<<"$opened")"
export SHADE_SOCKET="$(jq -r '.outcome.result.env.SHADE_SOCKET' <<<"$opened")"
cd "$(jq -r '.outcome.result.cwd' <<<"$opened")"
# A plain shell is not one of the agent names `open` looks for, so no keepalive
# was started: check `.outcome.result.keepalive.state`, and either bind one with
# `--owner-pid $$` or renew the lease yourself.
shade heartbeat
shade context
shade checkpoint --reason before-refactor
shade publish --branch agent/result --message 'Result'
# Done for now, but not done with the work: `sleep` frees the disk and
# `wake` gives it back. `release` is the only verb that ends a session.
shade sleep
shade wake --session task-42
shade release
```

The full command set is `open`, `attach`, `sleep`, `wake`, `status`, `context`, `heartbeat`, `checkpoint`, `fork`, `sync`, `restore`, `deps refresh`, `publish`, `resolve`, `release` and `events`, plus the administrative `warm`, `review resolve`, `doctor`, `gc` and `install`.

Mutation commands wait for their terminal domain outcome. A client deadline does not cancel daemon work: the structured timeout carries `operation` when known, and its `next` field preserves the exact idempotency key for a safe retry.

## Using it from a host

Install the TypeScript SDK from `packages/sdk-typescript`. It is typed and has no runtime dependencies.

```ts
import { ShadeClient } from "@shade/sdk";

const shade = new ShadeClient({
  socket: process.env.SHADE_SOCKET!,
  actor: { kind: "host", id: "my-orchestrator" },
});

const session = await shade.sessions.open(
  { session_id: task.id, repository: { kind: "local", path: "/absolute/repo" }, intent: task.title },
  { idempotency_key: `open:${task.id}` },
);

await spawnAgent({ cwd: session.cwd, env: session.env });
await session.checkpoint("turn-complete");
await session.publish({ branch: "agent/change", message: "agent change" });
await session.release();
```

The session handle heartbeats automatically and adopts successor handoffs for you, so your code normally observes only the final `cwd` and `env`. The Rust client crate `shade-client` mirrors the same facade: `client.sessions().open(..)` returns a session with `context`, `checkpoint`, `fork`, `sync`, `restore`, `refresh_dependencies`, `publish`, `resolve`, `sleep` and `release`, and `client.sessions().reattach(..)` / `.wake(..)` bring an idle or suspended one back -- on a session that is already live both hand back the handle you already hold rather than a second one heartbeating the same lease. What the daemon last said about the session is read through the handle, never cached: `session.lease()` and `session.workspace_id()` take one field, `session.opened_ref()` shares the whole snapshot as an `Arc` so `&session.opened_ref().env` outlives the expression, and `session.opened()` copies it when an owned value is what you want. Review decisions live on `client.reviews()`.

## Outcomes

Every mutation settles into one of four states.

| State | Meaning | What the caller does |
| --- | --- | --- |
| `completed` | The result is durable. | Read `outcome.result` and continue. |
| `accepted` | Execution continues on the daemon. | Retain `operation_id`, then poll it or resume events by cursor. |
| `review_required` | A secret change needs a decision. No values are present. | Show the key-only preview, then resolve with merge, keep or discard. |
| `conflict` | Integration could not complete. | Work only in the returned resolution workspace, then run `shade resolve` or `session.resolve(...)`. |

## Status

Version 0.1.0, release candidate. Honest limitations:

- The distribution is not signed or notarized. Integrity is verified with SHA-256 checksums only.
- The daemon is single-user and runs as your login user. It is **not** a same-UID process sandbox. A linked worktree exposes a writable Git object database, so the secret clean filter is an accidental-commit guardrail, not a boundary against a malicious local process. See [docs/SECURITY.md](docs/SECURITY.md) for the precise threat model.
- The SQLite schema is version 1 and stays there. A database written by a different schema version is rejected rather than converted.
- V1 rejects submodules, Git LFS, custom Git filters, tracked private `.env` files, missing dependency locks, source builds and executable package-manager configuration. Committed templates are not private files: a basename ending in `.example`, `.sample`, `.template`, `.dist` or `.defaults` -- `apps/web/.env.example` and the like -- is ordinary tracked content, while `.env`, `.env.local` and `.env.production` are still refused. Shade never installs a runtime or toolchain.
- Sleep preserves tracked content, untracked files and gitignored private files, but not gitignored build output: `node_modules`, `.venv` and anything like them are excluded from the checkpoint and rebuilt on wake from the shared dependency fingerprint. A wake also changes the workspace id and cwd, so a host that cached either must read them from the wake result.

See [docs/STATUS.md](docs/STATUS.md) for the current acceptance evidence.

## Repository layout

- `crates/shade-engine`: lifecycle, invariants, Git, APFS, SQLite, dependencies, secrets, GC and recovery.
- `crates/shade-protocol`: the versioned wire and domain model, with opaque Git object IDs.
- `crates/shade-client`: Rust SDK, also used by the CLI.
- `crates/shade-cli`: the single `shade` binary, serving both CLI and daemon.
- `crates/shade-cli/examples/release_gate.rs`: development-only evidence runner for APFS COW, space, context and CLI/IPC measurements.
- `packages/sdk-typescript`: the `@shade/sdk` TypeScript client.
- `packages/session-harness`: deterministic 20-session integration harness.
- `skills/shade-workspaces`: compact agent operating skill.
- `skills/shade-migrate-worktree`: agent skill for converting one `git worktree` into a Shade workspace.

## Development

```sh
cargo build --release
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets --no-fail-fast
bun run typecheck
bun test packages
bun run test:harness
```

`bun run test:harness` runs the harness with controlled tooling. `bun run test:harness:real` runs it against real host package managers. Several acceptance suites are `--ignored` by default because they need a live launchd domain or installed managers. [docs/RELEASE.md](docs/RELEASE.md) defines the full release gate: crash and GC matrices, malicious fixtures, the session harness, output budgets and recorded APFS performance and space evidence.

To package an accepted build:

```sh
python3 scripts/package_release.py --binary target/release/shade
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Security reports go through [SECURITY.md](SECURITY.md).

## Docs

- [Product requirements](docs/PRD.md)
- [Architecture](docs/ARCHITECTURE.md)
- [Migrating existing worktrees](docs/MIGRATING.md)
- [Protocol v1](docs/PROTOCOL.md)
- [Security model](docs/SECURITY.md)
- [Installation](docs/INSTALLATION.md)
- [Dependency configuration boundary](docs/DEPENDENCY-CONFIGURATION.md)
- [Script approvals](docs/SCRIPT-APPROVALS.md)
- [Diagnostics](docs/DIAGNOSTICS.md)
- [Release gate](docs/RELEASE.md)
- [Implementation status](docs/STATUS.md)

## License

MIT. See [LICENSE](LICENSE).
