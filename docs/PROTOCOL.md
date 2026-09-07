# Protocol v1

Transport is newline-delimited JSON over a per-user Unix socket with mode `0600`. One-shot requests receive one minified response. Subscriptions receive `EventEnvelope` JSONL records ordered by a monotonically increasing cursor.

Execute requests contain `v`, `request_id`, `idempotency_key`, `actor` and one tagged intent. The durable idempotency namespace is the actor tuple `(kind, id)`, not the display id alone; `operation_by_key` therefore carries both fields. Caller actor ids cannot contain NUL, which is reserved as the internal tuple separator. Query requests are read-only. Git object IDs are opaque strings.

Outcomes:

- `completed`: the result is durable.
- `accepted`: execution continues; retain `operation_id`.
- `review_required`: no secret values are present; resolve the review explicitly.
- `conflict`: work only in the returned resolution workspace, then send `resolution_complete` (`shade resolve`, or `session.resolve` in the SDKs); the outcome is a successor for the parent session.

Errors contain `code`, `retry`, optional `operation`, optional `next`, and optional `diagnostics_id`. They never embed command transcripts, secret fragments, access tokens, absolute tool paths, or a human-only fallback.

Completed values are nested at `outcome.result`:

```json
{"v":1,"request_id":"request-id","status":"ok","outcome":{"state":"completed","result":{}}}
```

Workspace selection uses `SHADE_WORKSPACE` first and otherwise the current directory. `open` returns `SHADE_SESSION`, `SHADE_LEASE`, `SHADE_WORKSPACE`, `SHADE_SOCKET`, `cwd`, and `compact_context` inside that result.

Successors use a durable two-phase handoff. The producing operation commits a small `PendingHandoff` while the predecessor and its lease remain active. A session may have exactly one pending handoff; competing successor preparation returns `HANDOFF_PENDING` with the handoff to adopt. After the caller receives it, the SDK or CLI sends `successor_adopt` with a stable `handoff:<id>` idempotency key. Adoption fences the predecessor, activates the successor and persists the final `OpenedSession` outcome in one SQLite transaction. Lost responses are recovered from the operation journal. If startup interrupted an adoption before that transaction, its operation reports `OPERATION_INTERRUPTED` and the same idempotency key reclaims the same operation ID safely. An expired predecessor cancels an unadopted successor. SDK façades perform adoption automatically, so applications normally observe only the final successor cwd/env.

Lease holders heartbeat every 30 seconds. TypeScript and Rust session handles do this automatically; standalone CLI owners use `shade heartbeat`. A lease expires after 120 seconds and becomes GC-eligible only after a further 10-minute grace. A transport timeout does not cancel its durable operation: retain `operation_id`, query it, or resume events strictly after the last persisted cursor. Both SDKs preserve the mutation idempotency key and perform a bounded `operation_by_key` lookup after an initial timeout; the timeout carries the recovered operation ID when available and always exposes the key for `operations.waitByKey` recovery.

The canonical model and exact serde tags live in [`crates/shade-protocol/src/lib.rs`](../crates/shade-protocol/src/lib.rs). Both SDKs consume this shape; the CLI does not define a second contract.

Script decisions use `dependency_script_decision` with `selector`, an exact `approval` tuple (`provider`, `package`, `version`, `integrity`) and `allow`. Their completed result is `{ "allowed": true, "refresh_required": true }` (or `false` after revocation). The `dependency_scripts` query returns recorded candidates with current `allowed` and artifact `executed` state. These detailed reports are separate from compact context. See [script approvals](SCRIPT-APPROVALS.md).
