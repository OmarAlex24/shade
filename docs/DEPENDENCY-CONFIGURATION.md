# Dependency configuration boundary

Shade validates dependency inputs before executing a manager, supplies a cleared
environment and records the exact installed toolchain. Approved project settings
remain part of the fingerprint. Configurations discovered outside that snapshot
must not change preparation. The isolation policy itself is fingerprinted, so a
policy change requires a new artifact or native-cache receipt.

## JavaScript

Identification and installation use an owned staging directory with private HOME,
XDG configuration directories, temporary storage and separate empty npm user and
global configuration files. The sanitized project `.npmrc` remains available.
npm normally discovers project, user and global files; both external file locations
are explicitly overridden. See [npm configuration sources](https://docs.npmjs.com/cli/v11/configuring-npm/npmrc/).

Executable load hooks, including `onload-script`, are rejected. Project settings
cannot redirect configuration, module/lock output, logs or stores outside the
owned layout. Ordinary settings such as `save-prefix` and scoped registries remain
valid. Frozen installation, disabled implicit scripts, installed-only manager
selection and OS network denial during replay continue to apply.

## Python

The uv provider requires a stable installed uv version of at least 0.11.16. That
version introduced `UV_NO_SYSTEM_CONFIG`; older versions would silently ignore the
requested boundary. Shade sets it for uv commands, uses private HOME and disables
executable keyring providers. Unsupported tool versions return a structured error;
Shade does not install another version. Approved `[tool.uv]` project settings are
still read from the validated staging snapshot.
See [uv environment variables](https://docs.astral.sh/uv/reference/environment/#uv_no_system_config).

## Cargo

Cargo can inherit `.cargo/config` and `.cargo/config.toml` from parent directories
and `CARGO_HOME`. Changing HOME alone does not stop that discovery.
See [Cargo's configuration hierarchy](https://doc.rust-lang.org/cargo/reference/config.html#hierarchical-structure).

Shade denies reads of these external configuration paths, including their resolved
symlink targets, using the macOS process sandbox during both fill and replay. The
validated repository `.cargo/config.toml` remains visible. A symlinked project
configuration or configuration directory is rejected, as are executable settings
and external configuration includes. RUSTC is the exact identified host executable;
the download cache and HOME are private. Preparation invokes fetch only.

## Go

Before each download/verification phase, the exact installed Go tool parses the
probe's metadata using `go mod edit -json` and `go work edit -json`. These commands
are read-only JSON inspections; Shade compares the input bytes again after parsing
and denies network access. See [Go module editing](https://go.dev/ref/mod#go-mod-edit).

Every local `use` or `replace` path must be relative, stay inside the COW probe
after lexical and symlink resolution, and identify a module with committed
metadata. Absolute and escaping paths are rejected. Internal sibling modules and
quoted paths containing spaces are accepted. Download, verify and offline replay
then run with automatic toolchain installation and user Go configuration disabled.
No source, build, tidy, vendor, workspace sync or generate command runs.

## Acceptance

The host suite includes adversarial npm global configuration, invalid uv system
configuration, Cargo parent executables/native-cache configuration and four forms
of external Go declarations. A positive Go workspace fixture exercises quoted
internal paths through the dependency service.

`cargo test -p shade --test configuration -- --ignored --nocapture` opens a real
polyglot repository through the default daemon and Rust SDK. It checks all four
provider receipts, unchanged lock metadata and blocked root/dependency/build code.
Set `SHADE_CONFIGURATION_BIN` to validate a particular distribution executable;
the emitted evidence includes its SHA-256 digest. These tests use owned temporary
configuration files and leave the user's configuration untouched.
