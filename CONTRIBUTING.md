# Contributing

## Platform requirement

Shade runs on **Apple Silicon, macOS and APFS only**. The CLI and daemon suites
are compiled behind `target_os = "macos"` and `target_arch = "aarch64"`, and
production materialization requires a real APFS clone syscall on the same
filesystem. There is no Linux, Intel Mac or non-APFS support, so a contribution
cannot be tested anywhere else yet. CI runs on `macos-15` for the same reason.

## Toolchain

- **Rust** — pinned by `rust-toolchain.toml` (1.93.1, with `clippy` and
  `rustfmt`). Install rustup and let it read the file; do not override the
  channel.
- **Bun** — the version in the `packageManager` field of `package.json`.
- **Git**, plus the macOS command line tools.
- Optional, only for the `--ignored` host-manager suites: npm/Node, pnpm/Node,
  Bun, uv (>= 0.11.16)/Python, Cargo/Rust and Go. Shade never installs a
  package manager or runtime, and neither do its tests.

## Local loop

Run only what your change can break:

```sh
cargo check -p shade-engine
cargo test -p shade-engine --test lifecycle
bun test packages
```

Crates are `shade-protocol`, `shade-engine`, `shade-client` and `shade` (the
single CLI/daemon binary). Suites are `lifecycle`, `recovery`, `gc`,
`git_filesystem`, `secret_documents` and `host_dependencies` under
`crates/shade-engine/tests/`, and `daemon_smoke`, `diagnostics`,
`secret_filter`, `configuration`, `installation`, `python_fork`,
`script_approvals` and `crash_matrix` under `crates/shade-cli/tests/`.

## Full gate before a pull request

```sh
cargo build --release
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets --no-fail-fast
bun run typecheck
bun test packages
bun run test:harness
```

## Ignored suites

Several acceptance suites are `#[ignore]` because they need installed host
package managers or a live launchd domain. Run them explicitly:

```sh
cargo test -p shade-engine --test host_dependencies -- --ignored --nocapture
cargo test -p shade --test configuration --test installation \
  --test python_fork --test script_approvals -- --ignored --nocapture
cargo test -p shade-engine --test git_filesystem -- --ignored --nocapture
```

`installation` bootstraps a fresh reserved LaunchAgent label in a temporary
root and never replaces the normal `com.shade.daemon` job. The real session
harness has the same shape and needs a release build:

```sh
SHADE_BIN="$PWD/target/release/shade" bun run test:harness:real
```

[docs/RELEASE.md](docs/RELEASE.md) is the authoritative gate and shows how to
run these against the packaged release binary with the evidence environment
variables set.

## Fault-injection crash matrix (optional)

```sh
CARGO_TARGET_DIR=/tmp/shade-fault-injection \
  cargo test -p shade --features fault-injection --test crash_matrix
```

Keep that build in its own target directory. The `fault-injection` feature is
test-only and must never be enabled in a distribution build.
`SHADE_CRASH_POINT` and `SHADE_CRASH_GROUP` narrow the matrix for diagnosis
only; a filtered run does not certify it.

## Commit messages

```
<type>: <description>
```

Types: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `perf`, `ci`.

## Design constraints

Respect these; a change that breaks one will be rejected regardless of tests.

- **No byte-copy fallback.** A failed APFS clone returns `COW_UNAVAILABLE`.
  Never add a copy path to "make it work" off APFS.
- **No secret values on the wire.** Reviews, events, responses and logs carry
  relative paths, key names and a classification — never values, fragments or
  hashes.
- **JSON-only CLI output.** Every command prints exactly one minified JSON
  value on stdout (`events --follow` prints JSONL). There is no prompt, color,
  table or human presentation mode.
- **Durable operations are journaled before side effects.** An operation row is
  inserted first, completion and its event commit together, and the result is
  idempotent per `(actor, key)` and recoverable at daemon startup.
- **Stay inside the V1 scope boundary** in [docs/PRD.md](docs/PRD.md):
  submodules, Git LFS, custom Git filters, tracked secret files, missing locks,
  source builds and executable package-manager configuration are rejected, and
  Shade never installs a runtime or toolchain.

Read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and
[docs/SECURITY.md](docs/SECURITY.md) before changing engine behaviour.

## Discussing design

Open a GitHub issue before a large or behavioural change, so the design can be
settled before the code. Security problems go through
[SECURITY.md](SECURITY.md), not a public issue.
