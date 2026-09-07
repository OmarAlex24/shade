# V1 release gate

## Package an accepted distribution

After complete acceptance, Python 3.9+ can package the exact accepted executable:

```sh
python3 scripts/package_release.py \
  --binary /tmp/shade-target/release/shade \
  --output-dir dist
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_*.py' -v
```

`scripts/package_release.py` checks `artifacts/v1-release-validation.json`, all
recorded source and evidence digests, the binary identity, skill budget and exact
APFS release profile and limits. A changed binary or accepted input is rejected;
new runtime builds need their own acceptance evidence. The command packages
existing accepted bytes and never rebuilds, installs or publishes the runtime.

The archive contains one executable plus its license, installation instructions,
skill, manifest, checksums and complete acceptance evidence. Tar entries have
fixed timestamps, modes and ownership; the gzip header has no source filename
or timestamp. Repeating with identical inputs and the same Python/zlib runtime
produces identical archive bytes. Use a different `--output-dir` for a repeat;
existing output files are never replaced. Packaging tests cover reproducibility,
executable mode, all payload checksums, modified inputs, incomplete acceptance,
threshold misses, unsafe paths and existing output preservation.

The local archive is unsigned and unnotarized, consistent with V1 scope. Follow
[installation](INSTALLATION.md#local-release-package) to verify it and install
the per-user service.

## Reproduce complete acceptance

Run every command from the repository root on Apple Silicon macOS with the Shade root on APFS. Stop on the first failure:

```sh
set -eu
test "$(uname -s)" = Darwin
test "$(uname -m)" = arm64
diskutil info "$(pwd)" | grep -q 'Type (Bundle):.*apfs'
export CARGO_TARGET_DIR=/tmp/shade-target

cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --no-fail-fast
mkdir -p artifacts
cargo test -p shade-engine --test host_dependencies -- --ignored --nocapture
CARGO_TARGET_DIR="$CARGO_TARGET_DIR/fault-injection" \
  SHADE_CRASH_REPORT="$PWD/artifacts/crash-lifecycle.json" \
  cargo test -p shade --features fault-injection --test crash_matrix
cargo build --release
SHADE_CONFIGURATION_BIN="$CARGO_TARGET_DIR/release/shade" \
SHADE_INSTALL_BIN="$CARGO_TARGET_DIR/release/shade" \
SHADE_PYTHON_FORK_BIN="$CARGO_TARGET_DIR/release/shade" \
  cargo test --release -p shade --test configuration --test installation \
    --test python_fork --test script_approvals -- --ignored --nocapture
cargo build -p shade --release --example shade-release-gate

mkdir -p artifacts
"$CARGO_TARGET_DIR/release/examples/shade-release-gate" \
  --mode smoke \
  --root /private/tmp \
  --output artifacts/apfs-smoke.json
"$CARGO_TARGET_DIR/release/examples/shade-release-gate" \
  --mode release \
  --root /private/tmp \
  --entries 25000 \
  --workspaces 20 \
  --latency-samples 100 \
  --payload-bytes 1024 \
  --output artifacts/apfs-release.json

bun install --frozen-lockfile
bun run typecheck
bun test packages
bun run test:harness
SHADE_BIN="$CARGO_TARGET_DIR/release/shade" bun run test:harness:real

test "$(wc -w < skills/shade-workspaces/SKILL.md)" -le 175
test "$(wc -c < skills/shade-workspaces/SKILL.md)" -le 1100
test -f LICENSE

retired_prefix='shadow'
retired_suffix='tree'
! rg -n -i "${retired_prefix}[-_ ]?${retired_suffix}" . --glob '!target/**' --glob '!.git/**'

retired_mode='leg''acy'
compat_root='compat''ib'
schema_history='migrat''ion'
! rg -n -i "\\b${retired_mode}\\b|${compat_root}(le|ility)|\\b${schema_history}s?\\b" crates packages skills README.md docs
```

Keep the
`fault-injection` build in its separate target directory; distribution builds
must omit that feature. The crash test starts isolated APFS pools, arms a named
phase, kills its own stopped daemon with `SIGKILL`, and checks recovery against
the same SQLite inode. It freezes a private copy of the instrumented executable
for every case in the run and records that copy's digest. `SHADE_CRASH_POINT` selects a point for diagnosis only;
a filtered artifact does not certify the full matrix. The artifact records
the instrumented binary digest and distinguishes intentionally retained
workspaces from leaks. Current named points cover lifecycle/control-plane
transactions and dependency artifacts. The npm cases cover fill, validation,
offline replay, approved scripts, promotion, COW replacement, receipt persistence
and interrupted LRU deletion. `SHADE_CRASH_GROUP=dependencies` selects this group
for diagnosis; a group-filtered record does not certify the full matrix. Cold
package cases have a 120-second rendezvous budget, separate from the APFS/IPC
latency gate. Provider filters `dependency-pnpm`, `dependency-bun`, `dependency-uv`,
`dependency-cargo` and `dependency-go` select the additional real-manager cases.
These exercise common artifact boundaries for each provider, Python bootstrap,
relocation and edited-environment forks, Cargo/Go cached offline replay and native
cache invalidation, and Go probe/download/verification phases. Fixtures pin
installed managers, use private registries where applicable, preserve predecessor
bytes, verify readiness after restart and finish with no live workspaces or leases.
The pnpm fixtures support an installed Corepack cache or a direct installation;
select the intended installed tool through PATH. No test installs a package manager.
On a failed case, the harness unloads its own daemon and preserves that fixture
directory as `SHADE_CRASH_FAILED_ROOT`, including private daemon diagnostics.
Operation failures include the persisted sanitized detail in the test failure;
`SHADE_ROOT=<retained-root>/state shade doctor --diagnostics <id>` retrieves it
after the failed daemon has stopped. The diagnostic/failure transaction also has
real SIGKILL cases before and after commit.
Six additional boundaries cover incremental-base index/tree/promotion phases.
Five cover publish-conflict allocation, integration and the atomic resolution
state/intent/outbox transaction. Each conflict case completes the retained publish
after restart and checks both branch targets. Sixteen more cases cover completion
of an already-resolved publication, including checkpoint, CAS, anchor cleanup and
handoff. `SHADE_CRASH_GROUP=resolved-publication` selects those cases.
`SHADE_CRASH_GROUP=reconciliation` selects fifteen startup-recovery cases. Each
first kills an operation and then kills its recovering daemon, preserving the same
SQLite inode through both restarts. The report records the seed point and actual
kill count. A lease-expiry case uses a one-second harness lease and elapsed time;
it never changes SQLite rows to manufacture expiry. It then proves the opposite
of collection: a garbage-collection round leaves the dormant workspace alone, and
only an explicit release reaches it. `SHADE_CRASH_GROUP=lifecycle` selects the six
suspension boundaries -- a sleep cut at each of its four commits and a wake at
both of its two -- each of which either retries the sleep from an intact tree,
finishes it from its checkpoint, or removes the orphan successor of an
interrupted wake. These group filters are diagnostic acceptance scopes and do
not replace the final complete matrix.
Inspect the reported failure before retrying, then remove that exact diagnostic
directory after the problem is resolved. Successful cases remove their fixtures.

The host dependency suite uses installed npm/Node, pnpm/Node, Bun, uv/Python, Cargo/Rust and Go on APFS. JS, Python and Cargo fixtures run against isolated local registries; Go downloads the pinned `golang.org/x/text v0.3.8` fixture from the public proxy. It exercises cold fills, sandboxed offline replay, JS workspace links, relocatable Python, blocked install/build/startup code, explicit JavaScript approvals/revocation and isolated script builds, and recovery from missing/corrupt caches. `SHADE_HOST_EVIDENCE` lines record receipt fingerprints and exact tool versions/digests. No manager or runtime is installed by Shade. The public CLI/daemon script workflow is also tested with `CARGO_TARGET_DIR=/tmp/shade-target cargo test -p shade --test script_approvals -- --ignored --nocapture`.

The [configuration acceptance](DEPENDENCY-CONFIGURATION.md) adds adversarial
external configuration and Go local-path cases to the fourteen-test host suite.
The release-binary polyglot test above checks all four ecosystem receipts through
the default daemon, unchanged lock metadata and blocked scripts. Preserve its
`SHADE_CONFIGURATION_EVIDENCE` record with the distribution binary digest.

The general TypeScript test command does not launch the real harness unless
`SHADE_BIN` is present. The explicit command above starts an isolated daemon
with Shade's hidden `--harness-lifecycle` override and a zero-second orphan
grace. It therefore exercises the real GC and cleanup path without changing
SQLite or waiting ten minutes. Daemons started normally retain the production
120-second lease TTL and 600-second orphan grace. The fixture uses the installed npm and Node with a local HTTP registry, a root plus nested dependency forest, and forbidden lifecycle hooks. Actual npm logs and Git Trace2 prove one fill/offline replay and one source fetch; the report binds tool identities and the frozen Shade binary digest.

The [operational installation test](INSTALLATION.md) installs the same release binary
under a fresh reserved LaunchAgent label in an isolated temporary root, opens a
real npm project, verifies KeepAlive after SIGKILL and unloads the service. It never
replaces the normal `com.shade.daemon` job. Preserve its `SHADE_INSTALL_EVIDENCE`
record with the binary digest.

`SHADE_PYTHON_FORK_BIN="$(pwd)/target/release/shade" cargo test -p shade --test python_fork -- --ignored --nocapture`
validates uv/Python preparation and fork through the real daemon and Rust SDK.
The child runs an edited package through its relocated entrypoint while the
parent `.venv` is temporarily unavailable; activation and parent bytes are also
checked. Preserve its `SHADE_PYTHON_FORK_EVIDENCE` record with the binary digest.

The word/byte limits are a conservative precheck. Record the selected production model tokenizer and its measured count; it must be at most 350 tokens.

`shade-release-gate` creates and removes an isolated pool beneath the selected
`--root` (the repository directory by default). The root must be APFS and have
room for twenty userspace byte-copy baselines. Release mode accepts only the
exact 25,000-entry, 20-workspace, 100-latency-sample, 1,024-byte profile shown
above. It measures a warm APFS clone distribution and fails with one minified
JSON object when any V1 latency, response-size or space threshold is missed. It
also starts the release-built `shade` daemon, opens a real Git workspace,
measures context over its Unix socket, and measures a fresh CLI process plus IPC
round trip. Use `--shade-bin` only when the two release binaries are not
siblings.

Smoke mode uses 64 files, three workspaces and five latency samples. It verifies
the real APFS, daemon and accounting paths but records
`"thresholds_enforced":false`; it is CI coverage, not performance evidence. It
still records every threshold miss and sets `"status":"failed"`, while leaving
the process exit successful so loaded CI hosts do not turn smoke into false
release evidence. Release mode accepts only the exact 25,000/20/100/1,024
profile shown above; smaller custom runs cannot certify V1.

The gate removes measured clone and byte-copy trees before starting its real
daemon phase. Its SDK operations use a separate fifteen-minute deadline so the
final 25,000-entry release checkpoint is not constrained by the normal agent
deadline. On every returned benchmark error, the isolated
`.shade-release-gate-*` pool is closed and removal is retried before the JSON
error is emitted. Error messages identify the failed phase and durable
operation ID when available; a cleanup failure also reports the exact retained
pool path.

The common-response budget serializes deterministic, minified wire fixtures for
context, checkpoint, heartbeat, publish, release, accepted and structured-error
outcomes, then adds the largest real context and doctor responses observed in
the run. The context fixture deliberately uses two opaque 64-character SHA-256
OIDs and a polyglot pnpm/uv dependency state. The measured byte count excludes
only the JSONL record delimiter; every sample must be at most 512 bytes.
Bootstrap `open`, event pages and review previews have variable payloads and are
not represented as common fixed-size responses.

The space comparison is based on the sum of
`ATTR_CMNEXT_PRIVATESIZE` (reported as `private_reclaimable_bytes`) for the COW
trees versus copies made with explicit userspace reads and writes. Logical and
referenced sizes are recorded too. A null ratio with a passing comparison means
the measured COW denominator was zero, not that a global shared-byte value was
estimated. The artifact also records hardware, macOS, filesystem, Git, Rust,
Shade, SHA-256 digests of the gate and Shade binaries when readable, sample
counts, raw samples and the declared warm state. Materialization latency is the
end-to-end `WorkspaceFilesystem::clone_immutable_tree` call, including the
inventory and exclusive durable publication. The immutable inventory reads
APFS directory entries for names, types and duplicate inodes, then validates
each directory through pinned descriptors. Unknown types, internal hardlinks
and more than 25,000 entries require the complete `getattrlistbulk` inventory
and controlled per-entry cloning. The fixture is an
immutable daemon-owned tree without hardlinks; mutable workspaces and
hardlinked trees retain the per-entry strategy. The current adapter does not
expose enumerate/clone/fsync/publish phase timings, so the gate does not
fabricate phase-level values. Never mark the hardware gate passed unless
`artifacts/apfs-release.json` says `"status":"passed"` for the production Apple
Silicon/APFS host.

## Required evidence

The automated gate is necessary but not sufficient. Attach one artifact per row; a missing artifact blocks the release.

| Artifact | Pass condition |
|---|---|
| Real-Git lifecycle test | Checkpoint/restore covers detached commits, staged, unstaged, deleted, untracked, symlink and executable state; the predecessor remains byte-identical. |
| Concurrency test | Twenty opens share one fetch, base and dependency preparation; mutating one workspace changes neither base, layer nor sibling. |
| Successor test | Sync, restore, refresh, conflict resolution and secret merge preserve predecessors until handoff. |
| Publish race test | Squash-only CAS leaves branches unmoved on conflict; concurrent publishers serialize; remote push is explicit and lease-protected. |
| Crash matrix | Kill after every filesystem, Git and SQLite phase; restart converges to zero resources or one complete resource. |
| GC matrix | Live leases, active operations, unmerged indexes, failed checkpoints, pending reviews and unanchored detached commits all block deletion. |
| Malicious dependency fixtures | JS scripts, `.pnpmfile`, PEP 517, `.pth`, `build.rs`, proc macros and Go project code never execute. Cold fill replays offline; corrupt layers rebuild. |
| Session harness | The deterministic protocol suite passes, then the real TypeScript SDK opens twenty sessions against the release binary, survives a SIGKILL restart with the same SQLite file, resumes events, checkpoints/forks/syncs/publishes, and uses an explicit zero-grace harness daemon to finish with zero active leases, worktrees, private refs, dependency staging entries or mutable artifact layers. |
| Operational installation | The distribution binary installs atomically, answers before installation succeeds, finds host npm/Node under launchd, recovers after SIGKILL with the same SQLite file/workspace, rejects acceptance-label collisions and unloads cleanly. |
| Output budgets | Every deterministic common-response fixture and every real context/doctor response sampled by the gate is at most 512 minified JSON bytes; the official Skill is at most 350 production-model tokens. |
| APFS benchmark | Warm source materialization at no more than 25k entries has p50 below 300 ms and p95 below 1 s; context p95 is below 200 ms; CLI/IPC p95 is below 10 ms. |
| Space benchmark | Twenty unchanged workspaces add at least five times less physical storage than twenty complete copies/installations. |

Benchmark artifacts record hardware, macOS, Git and tool versions, entry count, sample count, cold/warm state, and logical/referenced/private sizes. Performance numbers are measured release evidence, never unconditional claims.
