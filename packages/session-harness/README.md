# Session harness

A deterministic and a real 20-session harness that exercises the SDK the way a
host orchestrator would.

The fast deterministic harness is backed by a fake Shade daemon over a real
Unix-domain socket. It exercises protocol edge cases without requiring macOS:

```bash
bun run test:harness
```

The release harness drives the real TypeScript SDK and a real `shade` daemon. It
creates an npm monorepo fixture on APFS using the installed npm and Node; opens 20 concurrent sessions; observes automatic
heartbeats; proves those opens share one npm fingerprint and exactly one safe
fill/offline-replay layer; verifies COW dependency isolation across root and nested workspace forests; checkpoints,
forks, syncs and adopts a successor; publishes; kills
and restarts the daemon against the same SQLite file; resumes events by cursor;
releases every lease; and runs GC plus doctor before inspecting SQLite, managed
Git worktrees/private refs, dependency staging and artifact layers. Cleanup keeps
the expected immutable dependency layer while requiring zero receipts, staging,
mutable layer entries or workspace clones.

Tool versions and SHA-256 identities come from the installed executables and must match
the dependency receipt. A local HTTP registry serves two integrity-pinned tarballs
with forbidden lifecycle hooks. Native npm logs prove one safe online install plus
one safe offline replay; Git Trace2 proves one source fetch. A frozen copy of the
Shade binary runs every daemon phase and its digest is included in the report.

```bash
cargo build --release
SHADE_BIN="$PWD/target/release/shade" bun run test:harness:real
```

The harness starts its isolated daemon with the hidden, explicit
`--harness-lifecycle` timing override and a zero-second orphan grace. This
exercises the production GC predicates and filesystem cleanup without editing
SQLite or waiting ten minutes; an ordinary daemon still uses the fixed
120-second lease TTL and 600-second grace. Cleanup assertions are never
skipped. Set `SHADE_HARNESS_KEEP_ROOT=1` only while debugging a failure.
Ordinary `bun test packages` skips the real test when `SHADE_BIN` is absent.
Both executables emit one compact JSON record and no decorative output.
