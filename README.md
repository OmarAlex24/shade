# Shade

Shade is an agent-first workspace runtime for Apple Silicon Macs. It turns a Git repository into isolated APFS copy-on-write workspaces with durable leases, checkpoints, successor workspaces, dependency readiness, squash publishing, recovery, and resumable events.

V1 has one machine interface and one production target: Apple Silicon, macOS and APFS. Unsupported targets fail closed, and production never falls back to byte copies.

See the [implementation status](docs/STATUS.md) for completed V1 acceptance and measured release evidence.

## Package the accepted release

Create a local distribution from the binary identified by the accepted evidence:

```sh
python3 scripts/package_release.py --binary /tmp/shade-target/release/shade
```

The command requires Python 3.9+ and writes a reproducible archive and SHA-256
sidecar under `dist/`. It verifies the accepted source, binary, skill and evidence
before packaging the CLI/daemon, license, installation instructions and complete
acceptance artifacts. See [installation](docs/INSTALLATION.md) for verification
and installation, and [release packaging](docs/RELEASE.md#package-an-accepted-distribution)
for reproduction and checks.

## Build and test

Requirements: Apple Silicon, macOS, APFS, Rust 1.93+, Git, and whichever host package managers a repository declares.

Python repositories require stable uv >=0.11.16 for system-configuration isolation.
See the [dependency configuration boundary](docs/DEPENDENCY-CONFIGURATION.md) for
accepted project settings and external configuration handling.

```sh
cargo build --release
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets --no-fail-fast
bun run typecheck
bun test packages
bun run test:harness
```

Install the same binary as the CLI and per-user LaunchAgent:

```sh
./target/release/shade install
```

Every command emits one minified JSON value; `events --follow` emits JSONL. There is no prompt, color, table, or alternate presentation mode.

```zsh
open_key='open:zenith-chat-id'
opened="$(shade --idempotency-key "$open_key" open /absolute/repo --session zenith-chat-id)"
test "$(jq -r '.status' <<<"$opened")" = ok
test "$(jq -r '.outcome.state' <<<"$opened")" = completed
export SHADE_SESSION="$(jq -r '.outcome.result.env.SHADE_SESSION' <<<"$opened")"
export SHADE_WORKSPACE="$(jq -r '.outcome.result.env.SHADE_WORKSPACE' <<<"$opened")"
export SHADE_LEASE="$(jq -r '.outcome.result.env.SHADE_LEASE' <<<"$opened")"
export SHADE_SOCKET="$(jq -r '.outcome.result.env.SHADE_SOCKET' <<<"$opened")"
cd "$(jq -r '.outcome.result.cwd' <<<"$opened")"
# Zenith and both SDK session handles heartbeat automatically. A standalone
# CLI owner must run this at least every 30 seconds while it is alive:
shade heartbeat
shade context
shade checkpoint --reason before-refactor
shade publish --branch agent/result --message 'Result'
shade release
```

Mutation commands wait for their terminal domain outcome. A client deadline does
not cancel daemon work: the structured timeout includes `operation` when known
and its `next` field preserves the exact idempotency key for a safe retry.

Callers must retain the returned `cwd` and `SHADE_*` environment. Successors use a durable two-phase handoff: the predecessor remains live until the SDK/CLI confirms adoption, then cwd and environment change together. See the [product requirements](docs/PRD.md), [protocol](docs/PROTOCOL.md), [architecture](docs/ARCHITECTURE.md), [security model](docs/SECURITY.md), and [release gates](docs/RELEASE.md).

## Workspace

- `crates/shade-engine`: lifecycle, invariants, Git, APFS, SQLite, dependencies, secrets, GC and recovery.
- `crates/shade-protocol`: versioned wire/domain model with opaque Git OIDs.
- `crates/shade-client`: Rust SDK used by the CLI.
- `crates/shade-cli`: one `shade` binary and daemon.
- `crates/shade-cli/examples/release_gate.rs`: development-only APFS COW, space, context and CLI/IPC evidence runner; Shade still ships one binary.
- `packages/sdk-typescript`: Zenith-facing TypeScript SDK.
- `packages/zenith-harness`: deterministic 20-chat integration harness.
- `skills/shade-workspaces`: compact agent operating skill.

## Non-goals

V1 intentionally rejects non-APFS filesystems, custom Git filters, LFS, submodules, tracked `.env*`, missing dependency locks, source builds, and unsafe executable package-manager configuration. Shade never installs a runtime or toolchain.
