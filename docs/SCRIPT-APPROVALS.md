# JavaScript script approvals

`open` installs JavaScript dependencies with lifecycle scripts disabled. Shade records each installed registry package that declares `preinstall`, `install` or `postinstall`, including an implicit `node-gyp` install for `binding.gyp`. Workspace and root project hooks are excluded from reusable builds.

Inspect the candidates from a live workspace:

```sh
shade deps scripts --workspace WORKSPACE
```

Each entry contains the exact `provider`, `package`, `version` and `integrity` tuple, event names, `allowed` and `executed`. `allowed` describes the current decision; `executed` describes the artifact already installed in that workspace. These can differ after approval or revocation. The daemon reads candidates from its recorded receipts, so editing a workspace's `package.json` cannot manufacture an approval candidate. Aliases retain the canonical locked package name.

Copy the tuple from that response into an explicit decision:

```sh
shade deps approve-script --workspace WORKSPACE \
  --provider npm --package PACKAGE --version VERSION --integrity INTEGRITY
shade deps refresh --workspace WORKSPACE
```

Approval alone leaves the current workspace unchanged. Refresh returns a successor through the normal handoff flow. The CLI and both session facades complete adoption and return the successor cwd and environment.

Decisions apply to matching packages throughout the same private Shade installation. They persist across daemon restarts. Each decision, operation result and event commits in one SQLite transaction; `--idempotency-key` supports durable retry. A repository's `trustedDependencies`, manager approval metadata or script configuration does not grant permission.

Revoke with the same tuple:

```sh
shade deps revoke-script --workspace WORKSPACE \
  --provider npm --package PACKAGE --version VERSION --integrity INTEGRITY
shade deps refresh --workspace WORKSPACE
```

Revocation governs subsequent preparations; existing workspaces and artifacts keep their bytes. Relevant decisions, exact Node/shell identities and execution policy form part of the artifact fingerprint. A different package version or integrity requires its own decision.

Approved hooks run after frozen installation and offline replay, in dependency order. They receive independent COW files, an isolated home/temp directory and exact host Node and shell. macOS denies network access, reads outside the build/runtime allowlist, and writes outside that package's own files and temporary directories. Nested dependency trees remain protected; shared hardlinked files are detached with forced APFS clones before execution. Hook output stays out of public responses and logs. A failing hook blocks preparation; no partial build is promoted. Tools and native build prerequisites must already be available; Shade never downloads a toolchain or relaxes the offline sandbox for an approval.

For dynamically linked Node installations, Shade obtains loaded library paths from the installed runtime's [diagnostic API](https://nodejs.org/api/report.html), with a clean environment and network access denied. The sandbox permits those canonical library files individually; their names and contents contribute to the script artifact fingerprint. Library directories do not become readable. Script startup uses an empty OpenSSL configuration through [`OPENSSL_CONF`](https://nodejs.org/api/cli.html#openssl_conffile), so a host configuration outside the sandbox is not required.

The TypeScript facade provides `session.dependencyScripts()`, `approveScript(tuple)`, `revokeScript(tuple)` and `refreshDependencies()`. The Rust equivalents are `dependency_scripts`, `approve_script`, `revoke_script` and `refresh_dependencies`.
