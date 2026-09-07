# @shade/sdk

Typed, dependency-free TypeScript client for Shade protocol v1 over Unix-domain
socket NDJSON.

```ts
import { ShadeClient, ShadeTimeoutError } from "@shade/sdk";

const shade = new ShadeClient({
  socket: process.env.SHADE_SOCKET!,
  actor: { kind: "host", id: "<your-host-name>" },
});

const session = await shade.sessions.open(
  {
    session_id: chat.id,
    repository: { kind: "registered", repository_id: repository.id },
    intent: chat.title,
  },
  { idempotency_key: `open:${chat.id}` },
);

await launchAgent({ cwd: session.cwd, env: session.env });
await session.checkpoint("turn-complete");
await session.publish({ branch: "agent/change", message: "agent change" });
await session.release();
```

`sessions.open` returns the session object itself. Its machine-facing fields are
`session`, `workspace`, `lease`, `cwd`, `env`, and `compact_context`; its lifecycle
methods are `context`, `checkpoint`, `fork`, `sync`, `restore`,
`refreshDependencies`, `publish`, `resolve`, and `release`.

A `publish` that cannot integrate settles as `conflict` and returns a resolution
workspace. Fix the conflict there, then `resolve` it: the daemon publishes from
that workspace and returns a successor session, so the old handle is retired and
heartbeats move with it.

```ts
const outcome = await session.publish({ branch: "agent/change", message: "agent change" });
if (outcome.state === "conflict") {
  await launchAgent({ cwd: outcome.result.cwd, task: "resolve the conflict" });
  const next = await session.resolve(outcome.result);
  if (next.state === "completed") await next.result.release();
}
```

Mutations accept an optional explicit `idempotency_key`. Shade generates one when
omitted. Once the daemon answers `accepted`, the SDK only polls the returned
operation and never resends the intent. If the client deadline expires, the thrown
`ShadeTimeoutError` always retains the idempotency key. It also performs one bounded
lookup and includes `operation_id` when the daemon already journaled the mutation:

```ts
try {
  await session.checkpoint("turn-complete", { timeout_ms: 250 });
} catch (error) {
  if (error instanceof ShadeTimeoutError && error.operation_id) {
    await shade.operations.wait(error.operation_id);
  } else if (error instanceof ShadeTimeoutError && error.idempotency_key) {
    await shade.operations.waitByKey(error.idempotency_key);
  }
}
```

Event subscriptions reconnect automatically and resume strictly after their last
cursor:

```ts
for await (const event of shade.events(savedCursor, { signal })) {
  savedCursor = event.cursor;
}
```
