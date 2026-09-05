# V1 implementation and acceptance checklist

The user's original V1 plan is the acceptance scope. Passing the APFS benchmark or the twenty-chat harness alone does not complete it.

## Runtime and lifecycle

- [x] One Rust CLI/daemon binary; versioned private socket; SQLite WAL/FULL, operation journal and transactional outbox.
- [x] Persist sanitized diagnostics with operation failures; retrieve them through both SDKs and CLI, after restart and offline. Operational acceptance verifies the same diagnostic under launchd, after KeepAlive and after unloading.
- [x] Rust/TypeScript session facades, timeout recovery, idempotency and resumable events.
- [x] Canonical managed repositories, independent local imports, quarantined strict fetch, explicit refspecs and SHA-256 fixtures.
- [x] APFS-only staged materialization, controlled mutable clones and bounded immutable directory clones.
- [x] Detached locked worktrees, faithful checkpoints, fork, sync/restore/refresh, adoption, conflicts and squash publication with local/remote CAS.
- [x] Real crash tests for import/fetch, activation, registration, checkpoints, fork/restore, sync, handoffs, publish, release, secret decisions and GC.
- [x] Recovery of locked staging registrations before and after moving `.git`; temporary Git indexes use owned staging.
- [x] Atomic release and secret decisions; reviewed merge plus handoff, including already-adopted successors.
- [x] GC discovers late secrets and emits key-only reviews; decisions remain actionable after lease expiry.
- [x] Real SIGKILL before and after the diagnostic/failed-operation/outbox transaction; rollback and durable idempotent replay are verified.
- [x] Six incremental-base SIGKILL boundaries verify content, deletion, mode, symlink and file/directory transitions, old-base preservation and successful retry. Five publish-conflict boundaries verify atomic resolution state/intent/outbox and complete squash publication after restart. The state/intent split reproduced a lost-publication bug before the transaction fix.
- [x] Sixteen resolved-publication cases cover readiness, checkpoint, local/remote CAS, completion, anchor cleanup and handoff. Fifteen reconciliation cases use two actual SIGKILLs each, including real lease expiry, rollback/commit inspection before restart and a single durable recovered handoff.
- [x] Complete phase coverage for ecosystem-specific dependency paths. The complete matrix passes 251 cases across all 138 registered points, with 266 actual SIGKILLs, including 99 additional provider cases. [Recovery evidence](../artifacts/provider-recovery-validation.json) binds the complete run, source and instrumented binary.
- [x] Repeat complete functional and recovery acceptance against the distribution after the APFS inventory change: 145 ordinary Rust tests, twelve TypeScript tests, fourteen host-manager tests, four optimized CLI/LaunchAgent scenarios, twenty real sessions and all 251 crash cases pass. Retained diagnostics produced regressions for the earlier checkpoint, Node loader, registry and CLI timeout failures.

## Secret policy

- [x] `.env*` rejection at Git ingress/retention; private baselines, three-way previews, keep/discard/merge and predecessor preservation.
- [x] Detect secret content outside `.env*` at history ingress, Git add and checkpoint retention. A required persistent Git filter withholds rejected bytes; private JSON/TOML merges and opaque/binary reviews preserve originals.
- [x] Bind each review to an immutable private snapshot of parent/child bytes. Stale pending choices return a new review, and later edits invalidate completed cleanup decisions. SIGKILL coverage includes snapshot staging and publication.

## Dependency readiness

- [x] JS, Python/uv, Cargo and Go providers, fingerprints, receipts, staging, validation, offline replay and controlled-tool tests.
- [x] Real npm package preparation/reuse with an isolated HTTP registry, malicious lifecycle scripts and a corrupt-layer rebuild. The twenty-session harness uses real installed npm/Node, a local registry, root/nested workspace forests, actual npm logs and Git Trace2; it verifies one fill/replay, one source fetch and independent COW bytes.
- [x] Real cold-fill/offline-replay for all six package managers; JS workspace forests, hashed Python wheels, malicious install/build/startup fixtures and missing/corrupt caches. A dedicated subprocess test proves OS network denial even when the tool ignores offline flags.
- [x] Explicit script approvals bound to provider/package/version/integrity, durable transactional decisions, isolated COW builds and successor refresh. Real npm/pnpm/Bun tests cover approval, revocation and cache isolation; CLI/Rust tests cover restart, aliases and idempotency; TypeScript tests cover the wire tuple.
- [x] Resolve installed Corepack tools without fetching; include Node/Python interpreter identities; preserve pnpm metadata/policy-verification cache across replay; structurally parse Bun/pnpm/uv locks and Cargo/Bun config.
- [x] Reject executable configuration and automatic runtime declarations; verify Python bootstrap files against an empty environment created by the exact uv tool.
- [x] Isolate npm global/user configuration, uv system configuration and Cargo parent/native-cache configuration; reject executable npm load hooks and external Go workspace/replacement paths. Fourteen real-manager cases and a release-binary polyglot CLI-daemon test verify the boundary. The same release passes the Python fork, LaunchAgent and twenty-session harness; [configuration evidence](../artifacts/configuration-validation.json) records exact source/tool/binary digests.
- [x] Complete provider-specific recovery acceptance for all six managers: staging, validation, replay, promotion, COW replacement, receipts, LRU deletion, Python bootstrap/relocation/edited forks, Cargo/Go cache invalidation and cached replay, and Go probe/download/verification phases. Every case verifies readiness and appropriate cleanup after restart.
- [x] Dynamically linked Node can run approved hooks with exact library-file read permissions and private OpenSSL configuration. Library contents enter the artifact fingerprint; the regression preserves network, neighbor-file and write isolation. Fourteen host-manager tests and the release CLI approval/revocation workflow pass.

## Distribution and evidence

- [x] Both SDKs, compact skill and simulated/real Zenith harnesses.
- [x] Recorded M4 Pro/APFS latency, response and storage gates; skill measured at 239 tokens with the recorded tokenizer.
- [x] Test installation and LaunchAgent operation using an isolated service label without replacing an existing user installation. Verify actual npm readiness, KeepAlive after SIGKILL, the same SQLite inode/workspace and complete service unloading.
- [x] Bind lifecycle, security, dependency, concurrency, crash, operational and measured performance artifacts to the current source and distribution digests in [the current evidence bundle](../artifacts/v1-release-validation.json).
- [x] Pass every final performance threshold and complete V1 sign-off. The ordinary gate measures materialization p50 270.121 ms / p95 280.917 ms, context p95 47.430 ms and CLI/IPC p95 4.220 ms. Storage and response limits also pass. The bundle records `final_v1_release: true` and preserves the preceding failed gate with its host conditions.

Historical artifacts describe their recorded binary and do not certify subsequent source changes automatically. See [STATUS.md](STATUS.md) for observed results and [RELEASE.md](RELEASE.md) for release commands.
