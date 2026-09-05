# Installation and LaunchAgent acceptance

## Local release package

From the repository, package the already accepted binary with Python 3.9+:

```sh
python3 scripts/package_release.py --binary /tmp/shade-target/release/shade
```

The resulting files are `dist/shade-0.1.0-aarch64-apple-darwin.tar.gz` and its
`.sha256` sidecar. In that directory, verify the archive before extracting it:

```sh
cd dist
shasum -a 256 -c shade-0.1.0-aarch64-apple-darwin.tar.gz.sha256
tar -xzf shade-0.1.0-aarch64-apple-darwin.tar.gz
cd shade-0.1.0-aarch64-apple-darwin
shasum -a 256 -c SHA256SUMS
./shade --version
./shade install
```

The archive includes its accepted binary, MIT license, compact operating skill,
installation instructions and acceptance evidence. `MANIFEST.json` records
payload hashes and the accepted source/binary identities. Checksums verify
integrity; the V1 package is not signed or notarized. SDK source remains in the
repository. Creating or verifying a package does not run the installer.

## Installer behavior

Build on Apple Silicon macOS, then run `shade install` from the resulting binary.
Installation atomically copies that same executable to
`~/Library/Application Support/Shade/bin/shade`, writes a private plist at
`~/Library/LaunchAgents/com.zenith.shade.plist`, and bootstraps the user's GUI
launchd domain. The installed binary serves both CLI and daemon commands.
Installation returns success after the private socket answers `doctor`.

The plist supplies the selected Shade root and socket, a restrictive umask,
KeepAlive, and a canonical snapshot of existing host PATH directories. Shade
does not install package managers or runtimes. Reinstall after changing the
directories used to discover tools. The providers still bind each preparation
to the actual tool and interpreter identities they resolve.

The service uses `ProcessType=Interactive`: SDK clients wait on its Unix socket,
so XPC activity cannot raise an Adaptive job's priority. Background CPU/I/O
throttling caused a tiny fixture import to exceed the SDK deadline during the
operational test. Apple's [launchd plist reference](https://github.com/apple-oss-distributions/launchd/blob/main/man/launchd.plist.5)
describes those scheduling classes.

## Isolated acceptance

```sh
CARGO_TARGET_DIR=/tmp/shade-target cargo build --release
CARGO_TARGET_DIR=/tmp/shade-target \
  SHADE_INSTALL_BIN=/tmp/shade-target/release/shade \
  cargo test -p shade --test installation -- --ignored --nocapture
```

This explicit test requires an active GUI launchd domain and installed npm/Node.
It invokes the actual installer with hidden acceptance arguments, a fresh
`com.zenith.shade.acceptance.<id>` label and a temporary root. The installer
rejects a label collision before writing files and restricts acceptance state,
socket, binary and plist locations to that root. The normal user service is
never selected by these arguments.

The test verifies binary identity, private plist/socket permissions, host npm
readiness and blocked lifecycle hooks. It kills its own service with SIGKILL,
waits for KeepAlive, and verifies the same SQLite inode and workspace. Finally
it unloads the service, waits for launchd to reap it and removes its temporary
files. It also creates a failure through the installed daemon and verifies its
private diagnostic through the Rust SDK, after KeepAlive restart and through
`doctor --diagnostics` after unloading. `SHADE_INSTALL_EVIDENCE` records the binary digest, process identities,
open/restart timings and completed checks. `SHADE_INSTALL_KEEP_ROOT=1` retains
only the test files for diagnosis; its service is still unloaded on failure.

This operational evidence is separate from the APFS latency gate and must be
recorded again for the final distribution binary.
