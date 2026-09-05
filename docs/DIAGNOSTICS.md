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
