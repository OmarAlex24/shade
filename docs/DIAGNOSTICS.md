# Durable error diagnostics

An internal failure returns a compact error with `diagnostics_id`. Retrieve its
detail explicitly:

```sh
shade doctor --diagnostics diag_01J00000000000000000000000
```

The response is a completed outcome containing `id`, `origin`, `operation`,
`code`, `message`, `redacted`, `truncated` and `created_at_ms`. The origin is
`daemon` or `cli`; a local transport/startup failure may have no operation.
Ordinary `shade doctor` continues to report runtime health. Diagnostic details
are excluded from ordinary error responses and event payloads.

Both SDKs expose `client.diagnostics(id)`. The protocol query is
`{"kind":"diagnostics","diagnostics_id":"..."}`. Missing and malformed IDs
return `DIAGNOSTIC_NOT_FOUND` and `DIAGNOSTIC_ID_INVALID` respectively.

The CLI first queries its configured daemon. If that socket is absent or
refuses the connection, it reads the existing `state.sqlite` in the configured
Shade root. Set `SHADE_ROOT` to the root that produced the error when using an
isolated runtime. An offline lookup does not create state, start a daemon or
reconcile workspaces. SDK lookups use IPC and require the daemon.

Failure details, the operation's failed state and its outbox event commit in
one SQLite transaction. Retrying a completed failure with the same actor and
idempotency key preserves the diagnostic reference. A restart preserves the
record. Errors before an operation is claimed and local CLI failures are also
recorded when storage is available. If diagnostic storage fails, Shade omits
the reference and retains any known operation ID for recovery.

SQLite files use mode `0600`; the Shade root uses `0700`. Before persistence,
detected secret signatures, credential assignments, authorization/cookie
headers and credential-bearing URLs cause the whole message to be replaced
with a redaction marker. This check precedes truncation, including for multiline
private keys. Other messages are limited to 8,192 UTF-8 bytes. Native command
errors may still include local paths and non-secret command context, so these
records remain private runtime state. They are retained with the operation
journal and are not automatically exported.

Acceptance covers CLI lookup while running, after restart and offline; typed
Rust and TypeScript lookups; secret-safe database/WAL contents; failed writes;
and transaction rollback. Real SIGKILL cases at `DiagnosticWritten`,
`OperationFailureWritten` and `OperationFailed` distinguish rollback before
commit from a durable failure after commit. The isolated LaunchAgent test also
queries the same record after KeepAlive restart and after unloading the service.

## Parked tier reporting

`shade doctor` reports the tier's configuration and what it holds.
`park_root` is the configured volume, or `null` when the tier is off;
`park_mounted` answers whether that directory is present and writable *right
now*, which is a question about the moment rather than about the config;
`parks` and `park_bytes` count the park records and the bytes they claim; and
`parks_orphaned` counts the records that no longer describe a live suspension
and are therefore the collector's. Every byte count comes from the records, so
`doctor` never walks a volume that may not be plugged in. In `--human` mode an
unconfigured root prints as `-`, and a root that is configured while
`park_mounted` is false prints one warning: until the volume is back, every
sleep discards the build output it would have kept. `shade status --human` adds
a `PARKED` column holding the bytes parked for the checkpoint each workspace
would wake from, and `-` when there are none.

`shade gc` reports what one pass did to the parks. `parks_deleted` are garbage
parks taken off a mounted volume together with their records.
`parks_retained` are garbage parks left where they are, because the volume is
not mounted or because a removal failed and forgetting the record would strand
bytes nothing knows the name of. `parks_orphans_removed` are directories on a
mounted volume that no record claimed. `park_records_dropped` are records whose
directory is gone from a mounted volume. All four are zero when no park root is
configured, and only the mounted-volume cases ever remove anything.

A park commits its record and a `workspace.parked` event in one transaction.
The event carries `workspace_id`, `checkpoint_id`, `park_path` and `bytes`:
names and a size, never a file list.

`sleep` returns `parked`, `parked_bytes` and, when it parked, `park_path`.
`park_reason` says why it did not.

| `park_reason` on `sleep` | Meaning |
| --- | --- |
| `unconfigured` | No `SHADE_PARK_ROOT`. The tier is off. |
| `unmounted` | The configured root is not a writable directory right now. |
| `below_min_bytes` | The tree is smaller than `SHADE_PARK_MIN_BYTES`. |
| `park_failed` | The copy or the volume failed. The sleep did not. |
| `not_recorded` | The copy landed but its record did not, so the bytes were taken back while the volume was still there. |

`wake` returns `park_restored` and `park_restored_bytes`. `park_reason` is
absent when there was no park recorded for the suspension checkpoint and when
the park came back; otherwise it says what stopped it.

| `park_reason` on `wake` | Meaning |
| --- | --- |
| `unconfigured` | A park is recorded, but this daemon has no park root. |
| `unmounted` | The volume is not there right now. |
| `absent` | The park directory or its manifest is gone. |
| `manifest_mismatch` | The manifest describes another workspace, checkpoint, HEAD, worktree or layout version. |
| `park_failed` | The copy back failed. The wake did not. |

Only the case that earns it also fills `next`: a park existed and its bytes did
not come back, so the successor is a correct tree that will still take a full
build. Nothing else about the wake changes.
