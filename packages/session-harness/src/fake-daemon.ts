import {
  existsSync,
  mkdirSync,
  rmSync,
  unlinkSync,
} from "node:fs";
import { join } from "node:path";
import {
  createServer,
  type Server,
  type Socket,
} from "node:net";

import {
  PROTOCOL_VERSION,
  type Actor,
  type CompactContext,
  type EventEnvelope,
  type Intent,
  type JsonValue,
  type OpenedSessionPayload,
  type OperationOutcome,
  type OperationRecord,
  type PendingHandoffPayload,
  type Query,
  type ReviewAction,
  type SessionStatus,
  type ReviewId,
  type ShadeErrorBody,
  type WireRequest,
  type WorkspaceSelector,
} from "../../sdk-typescript/src/index.ts";

interface FakeSession {
  opened: OpenedSessionPayload;
  released: boolean;
  /** Slept: the record is whole, the tree is gone, there is no lease. */
  suspended: boolean;
  expires_at_ms: number;
  secret_review: boolean;
}

interface FakeReview {
  workspace: string;
  session: string;
  action?: ReviewAction;
}

interface FakeHandoff {
  actor: string;
  parent: FakeSession;
  successor: OpenedSessionPayload;
  adopted: boolean;
}

/** A resolution workspace handed back by a `conflict` publish, awaiting `resolution_complete`. */
interface FakeResolution {
  parent: FakeSession;
  successor: OpenedSessionPayload;
}

class FakeRequestError extends Error {
  constructor(readonly body: ShadeErrorBody) {
    super(body.code);
  }
}

let daemonSequence = 0;

export class FakeShadeDaemon {
  readonly root: string;
  readonly socket: string;
  readonly lease_ttl_ms: number;

  operation_query_delay_ms = 0;
  selector_checks = 0;
  heartbeat_count = 0;
  readonly idempotency_keys = new Set<string>();
  readonly heartbeat_counts = new Map<string, number>();

  private server: Server | undefined;
  private readonly sockets = new Set<Socket>();
  private readonly subscribers = new Set<Socket>();
  private readonly sessions = new Map<string, FakeSession>();
  private readonly operations = new Map<string, OperationRecord<unknown>>();
  private readonly operations_by_key = new Map<string, string>();
  private readonly checkpoints = new Map<string, string>();
  private readonly reviews = new Map<ReviewId, FakeReview>();
  private readonly handoffs = new Map<string, FakeHandoff>();
  private readonly resolutions = new Map<string, FakeResolution>();
  private readonly events: EventEnvelope[] = [];
  private readonly timers = new Set<ReturnType<typeof setTimeout>>();

  private workspace_sequence = 0;
  private lease_sequence = 0;
  private operation_sequence = 0;
  private checkpoint_sequence = 0;
  private review_sequence = 0;
  private handoff_sequence = 0;
  private object_sequence = 0;

  constructor(leaseTtlMs = 60) {
    daemonSequence += 1;
    this.root = join(
      "/tmp",
      `shade-session-${process.pid}-${daemonSequence.toString(36)}`,
    );
    this.socket = join(this.root, "d.sock");
    this.lease_ttl_ms = leaseTtlMs;
  }

  /**
   * Expires a lease on demand so a test can reach dormancy without waiting out
   * a TTL. The session survives: only its lease is gone.
   */
  expire(session_id: string): void {
    const session = this.sessions.get(session_id);
    if (session === undefined) throw new Error(`no session ${session_id}`);
    session.expires_at_ms = 0;
  }

  get active_leases(): number {
    let count = 0;
    for (const session of this.sessions.values()) {
      if (!session.released && session.expires_at_ms >= Date.now()) count += 1;
    }
    return count;
  }

  get resource_count(): number {
    return this.active_leases;
  }

  get latest_cursor(): number {
    return this.events.at(-1)?.cursor ?? 0;
  }

  async start(): Promise<void> {
    if (this.server !== undefined) return;
    mkdirSync(this.root, { recursive: true });
    this.server = createServer((socket) => this.accept(socket));
    await new Promise<void>((resolve, reject) => {
      this.server!.once("error", reject);
      this.server!.listen(this.socket, () => resolve());
    });
  }

  disconnectSubscribers(): void {
    for (const socket of this.subscribers) socket.destroy();
    this.subscribers.clear();
  }

  async crashAndRestart(): Promise<void> {
    for (const socket of this.sockets) socket.destroy();
    this.sockets.clear();
    this.subscribers.clear();
    const server = this.server;
    this.server = undefined;
    if (server !== undefined) {
      await new Promise<void>((resolve) => server.close(() => resolve()));
    }
    if (existsSync(this.socket)) unlinkSync(this.socket);
    await this.start();
  }

  async stop(): Promise<void> {
    for (const timer of this.timers) clearTimeout(timer);
    this.timers.clear();
    for (const socket of this.sockets) socket.destroy();
    this.sockets.clear();
    this.subscribers.clear();

    const server = this.server;
    this.server = undefined;
    if (server !== undefined) {
      await new Promise<void>((resolve) => server.close(() => resolve()));
    }
    if (existsSync(this.root)) rmSync(this.root, { recursive: true, force: true });
  }

  private accept(socket: Socket): void {
    this.sockets.add(socket);
    socket.setEncoding("utf8");
    let input = "";
    let handled = false;

    socket.on("data", (chunk: string) => {
      if (handled) return;
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline < 0) return;
      handled = true;
      const line = input.slice(0, newline);
      void this.handle(socket, line);
    });
    socket.on("close", () => {
      this.sockets.delete(socket);
      this.subscribers.delete(socket);
    });
    socket.on("error", () => undefined);
  }

  private async handle(socket: Socket, line: string): Promise<void> {
    let request: WireRequest;
    try {
      request = JSON.parse(line) as WireRequest;
    } catch {
      this.write(socket, {
        v: PROTOCOL_VERSION,
        request_id: "invalid",
        status: "error",
        error: { code: "INVALID_NDJSON", retry: "never" },
      });
      return;
    }

    try {
      if (request.v !== PROTOCOL_VERSION) {
        throw new FakeRequestError({
          code: "PROTOCOL_VERSION",
          retry: "never",
        });
      }
      if (request.type === "subscribe") {
        this.subscribe(socket, request.after_cursor);
        return;
      }
      if (request.type === "execute") {
        this.execute(
          socket,
          request.request_id,
          request.idempotency_key,
          request.actor,
          request.intent,
        );
        return;
      }
      this.query(socket, request.request_id, request.query);
    } catch (error) {
      const body =
        error instanceof FakeRequestError
          ? error.body
          : { code: "FAKE_INTERNAL", retry: "never" };
      this.respondError(socket, request.request_id, body);
    }
  }

  private execute(
    socket: Socket,
    requestId: string,
    idempotencyKey: string,
    actor: Actor,
    intent: Intent,
  ): void {
    if (idempotencyKey.length === 0 || idempotencyKey.length > 256) {
      throw new FakeRequestError({
        code: "INVALID_IDEMPOTENCY_KEY",
        retry: "never",
      });
    }
    const lookupKey = `${actor.id}\u0000${idempotencyKey}`;
    const existingId = this.operations_by_key.get(lookupKey);
    const existing =
      existingId === undefined ? undefined : this.operations.get(existingId);
    if (existing?.outcome !== null && existing?.outcome !== undefined) {
      this.respond(socket, requestId, existing.outcome);
      return;
    }
    this.idempotency_keys.add(idempotencyKey);
    switch (intent.kind) {
      case "session_open": {
        if (this.sessions.has(intent.session_id)) {
          throw new FakeRequestError({
            code: "SESSION_EXISTS",
            retry: "never",
          });
        }
        const opened = this.createSession(intent.session_id, intent.base);
        this.emit("session_opened", opened.workspace, {
          session: opened.session,
          lease: opened.lease,
        });
        this.completeOperation(
          socket,
          requestId,
          { state: "completed", result: opened },
          intent.kind,
          idempotencyKey,
          actor.id,
        );
        return;
      }
      case "session_reattach": {
        const session = this.sessions.get(intent.session_id);
        if (session === undefined) {
          throw new FakeRequestError({
            code: "SESSION_NOT_FOUND",
            retry: "never",
          });
        }
        if (session.released) {
          throw new FakeRequestError({
            code: "SESSION_ALREADY_RELEASED",
            retry: "never",
          });
        }
        const opened = this.renewLease(session);
        this.emit("session_reattached", opened.workspace, {
          session: opened.session,
          lease: opened.lease,
        });
        this.completeOperation(
          socket,
          requestId,
          { state: "completed", result: opened },
          intent.kind,
          idempotencyKey,
          actor.id,
        );
        return;
      }
      case "workspace_sleep": {
        const session = this.select(intent.selector);
        const checkpointId = this.next("ckpt", ++this.checkpoint_sequence);
        session.suspended = true;
        session.expires_at_ms = 0;
        // The tree is what sleep gives up; everything else survives.
        rmSync(session.opened.cwd, { recursive: true, force: true });
        session.opened = {
          ...session.opened,
          compact_context: {
            ...session.opened.compact_context,
            lease: "released",
            lifecycle: "suspended",
            changes: { staged: 0, unstaged: 0, untracked: 0 },
          },
        };
        this.emit("workspace_suspended", session.opened.workspace, {
          session: session.opened.session,
          checkpoint: checkpointId,
        });
        this.completeOperation(
          socket,
          requestId,
          {
            state: "completed",
            result: {
              session: session.opened.session,
              workspace: session.opened.workspace,
              checkpoint_id: checkpointId,
              suspended: true,
              reclaimed_bytes: 4096,
            },
          },
          intent.kind,
          idempotencyKey,
          actor.id,
        );
        return;
      }
      case "session_wake": {
        const session = this.sessions.get(intent.session_id);
        if (session === undefined) {
          throw new FakeRequestError({
            code: "SESSION_NOT_FOUND",
            retry: "never",
          });
        }
        if (session.released) {
          throw new FakeRequestError({
            code: "SESSION_ALREADY_RELEASED",
            retry: "never",
          });
        }
        // Waking a session that never slept just resumes it, which is what
        // makes `wake` safe to call unconditionally.
        const opened = session.suspended
          ? this.wakeSuspended(session)
          : this.renewLease(session);
        this.emit(
          session.suspended ? "session_woken" : "session_reattached",
          opened.workspace,
          { session: opened.session, lease: opened.lease },
        );
        session.suspended = false;
        this.completeOperation(
          socket,
          requestId,
          { state: "completed", result: opened },
          intent.kind,
          idempotencyKey,
          actor.id,
        );
        return;
      }
      case "workspace_checkpoint": {
        const session = this.select(intent.selector);
        const checkpointId = this.next("cp", ++this.checkpoint_sequence);
        this.checkpoints.set(checkpointId, session.opened.session);
        this.emit("checkpoint_created", session.opened.workspace, {
          checkpoint_id: checkpointId,
          reason: intent.reason,
        });
        this.acceptOperation(
          socket,
          requestId,
          {
            checkpoint_id: checkpointId,
            head_sha: session.opened.compact_context.head_sha,
            index_tree: this.objectId(),
            working_tree: this.objectId(),
          },
          intent.kind,
          idempotencyKey,
          actor.id,
        );
        return;
      }
      case "workspace_fork": {
        const parent = this.select(intent.selector);
        if (this.sessions.has(intent.child_session_id)) {
          throw new FakeRequestError({
            code: "SESSION_EXISTS",
            retry: "never",
          });
        }
        const child = this.createSession(
          intent.child_session_id,
          parent.opened.compact_context.head_sha,
        );
        this.emit("session_forked", child.workspace, {
          parent: parent.opened.session,
          child: child.session,
          intent: intent.intent ?? null,
        });
        this.acceptOperation(
          socket,
          requestId,
          child,
          intent.kind,
          idempotencyKey,
          actor.id,
        );
        return;
      }
      case "workspace_sync": {
        const parent = this.select(intent.selector);
        const handoff = this.createSuccessor(parent, actor.id);
        this.emit("workspace_synced", handoff.successor, {
          predecessor: parent.opened.workspace,
        });
        this.acceptOperation(
          socket,
          requestId,
          handoff,
          intent.kind,
          idempotencyKey,
          actor.id,
        );
        return;
      }
      case "workspace_restore": {
        const parent = this.select(intent.selector);
        if (!this.checkpoints.has(intent.checkpoint_id)) {
          throw new FakeRequestError({
            code: "CHECKPOINT_NOT_FOUND",
            retry: "never",
          });
        }
        const handoff = this.createSuccessor(parent, actor.id);
        this.emit("workspace_restored", handoff.successor, {
          predecessor: parent.opened.workspace,
          checkpoint_id: intent.checkpoint_id,
        });
        this.acceptOperation(
          socket,
          requestId,
          handoff,
          intent.kind,
          idempotencyKey,
          actor.id,
        );
        return;
      }
      case "dependencies_refresh": {
        const parent = this.select(intent.selector);
        const handoff = this.createSuccessor(parent, actor.id);
        const pending = this.handoffs.get(handoff.handoff_id)!;
        pending.successor.compact_context.dependencies = {
          state: "ready",
          providers: ["python"],
        };
        this.emit("dependencies_ready", handoff.successor, {
          predecessor: parent.opened.workspace,
          layer: `dep-${handoff.successor}`,
        });
        this.acceptOperation(
          socket,
          requestId,
          handoff,
          intent.kind,
          idempotencyKey,
          actor.id,
        );
        return;
      }
      case "workspace_publish": {
        const session = this.select(intent.selector);
        if (intent.branch === "conflict") {
          const operationId = this.operationId();
          const resolution = this.buildSession(session.opened.session, this.objectId());
          resolution.compact_context.dependencies = {
            ...session.opened.compact_context.dependencies,
          };
          this.resolutions.set(resolution.workspace, {
            parent: session,
            successor: resolution,
          });
          this.emit("publish_conflict", resolution.workspace, {
            operation_id: operationId,
            predecessor: session.opened.workspace,
          });
          const outcome: OperationOutcome<unknown> = {
            state: "conflict",
            result: {
              operation_id: operationId,
              workspace: resolution.workspace,
              cwd: resolution.cwd,
              paths: ["src/conflict.ts"],
            },
          };
          this.storeOperation(
            operationId,
            outcome,
            intent.kind,
            idempotencyKey,
            actor.id,
          );
          this.respond(socket, requestId, outcome);
          return;
        }
        if (intent.branch === "needs-review") {
          session.secret_review = true;
        }
        const commit = this.objectId();
        this.emit("workspace_published", session.opened.workspace, {
          branch: intent.branch,
          commit,
        });
        this.acceptOperation(
          socket,
          requestId,
          {
            branch: intent.branch,
            commit,
            tree: this.objectId(),
            previous_remote: null,
            pushed: intent.push ?? false,
          },
          intent.kind,
          idempotencyKey,
          actor.id,
        );
        return;
      }
      case "workspace_release": {
        const session = this.select(intent.selector);
        if (session.secret_review && !session.released) {
          const reviewId = this.next("review", ++this.review_sequence);
          this.reviews.set(reviewId, {
            workspace: session.opened.workspace,
            session: session.opened.session,
          });
          this.emit("review_required", session.opened.workspace, {
            review_id: reviewId,
          });
          this.completeOperation(
            socket,
            requestId,
            {
            state: "review_required",
            result: {
              review_id: reviewId,
              kind: "secret_cleanup",
              files: [
                {
                  path: ".env",
                  file_result: "child",
                  keys: [{ key: "TOKEN", result: "conflict" }],
                },
              ],
            },
            },
            intent.kind,
            idempotencyKey,
            actor.id,
          );
          return;
        }
        if (!session.released) {
          session.released = true;
          rmSync(session.opened.cwd, { recursive: true, force: true });
          this.emit("workspace_released", session.opened.workspace, {
            session: session.opened.session,
          });
        }
        this.acceptOperation(
          socket,
          requestId,
          {
            session: session.opened.session,
            workspace: session.opened.workspace,
            checkpoint_id: null,
            released: true,
          },
          intent.kind,
          idempotencyKey,
          actor.id,
        );
        return;
      }
      case "review_resolve": {
        const review = this.reviews.get(intent.review_id);
        if (review === undefined) {
          throw new FakeRequestError({
            code: "REVIEW_NOT_FOUND",
            retry: "never",
          });
        }
        review.action = intent.action;
        this.emit("review_resolved", intent.review_id, {
          action: intent.action,
        });
        const session = this.sessions.get(review.session);
        if (session === undefined) {
          throw new FakeRequestError({
            code: "WORKSPACE_NOT_FOUND",
            retry: "never",
          });
        }
        if (intent.action === "merge_parent") {
          const handoff = this.createSuccessor(session, actor.id);
          this.acceptOperation(
            socket,
            requestId,
            handoff,
            intent.kind,
            idempotencyKey,
            actor.id,
          );
          return;
        }
        session.released = true;
        rmSync(session.opened.cwd, { recursive: true, force: true });
        this.acceptOperation(
          socket,
          requestId,
          intent.action === "discard"
            ? {
                review: intent.review_id,
                resolution: "discarded",
                released: review.workspace,
              }
            : {
                review: intent.review_id,
                resolution: "kept",
                workspace: review.workspace,
              },
          intent.kind,
          idempotencyKey,
          actor.id,
        );
        return;
      }
      case "successor_adopt": {
        const handoff = this.handoffs.get(intent.handoff_id);
        if (handoff === undefined) {
          throw new FakeRequestError({ code: "HANDOFF_NOT_FOUND", retry: "never" });
        }
        if (handoff.actor !== actor.id) {
          throw new FakeRequestError({ code: "HANDOFF_FORBIDDEN", retry: "never" });
        }
        if (!handoff.adopted) {
          handoff.parent.released = true;
          rmSync(handoff.parent.opened.cwd, { recursive: true, force: true });
          this.sessions.set(handoff.successor.session, {
            opened: handoff.successor,
            released: false,
            suspended: false,
            expires_at_ms: Date.now() + this.lease_ttl_ms,
            secret_review: false,
          });
          handoff.adopted = true;
          this.emit("successor_activated", handoff.successor.workspace, {
            handoff: intent.handoff_id,
            predecessor: handoff.parent.opened.workspace,
          });
        }
        this.completeOperation(
          socket,
          requestId,
          { state: "completed", result: handoff.successor },
          intent.kind,
          idempotencyKey,
          actor.id,
        );
        return;
      }
      case "lease_heartbeat": {
        const session = this.sessions.get(intent.session_id);
        if (session === undefined || session.released) {
          throw new FakeRequestError({ code: "LEASE_EXPIRED", retry: "never" });
        }
        if (session.opened.lease !== intent.lease_id) {
          throw new FakeRequestError({ code: "LEASE_FENCED", retry: "never" });
        }
        // Sleeping releases the lease too, so a suspension would otherwise be
        // indistinguishable from a dormancy -- and send the caller into a
        // reattach that cannot succeed. Name it, and the command that ends it.
        if (session.suspended) {
          throw new FakeRequestError({
            code: "SESSION_SUSPENDED",
            retry: "never",
            next: `shade wake --session ${intent.session_id}`,
          });
        }
        // An expired lease leaves the session dormant, not released: the
        // workspace survives and a `session_reattach` brings it back.
        if (session.expires_at_ms < Date.now()) {
          throw new FakeRequestError({ code: "LEASE_EXPIRED", retry: "never" });
        }
        session.expires_at_ms = Date.now() + this.lease_ttl_ms;
        this.heartbeat_count += 1;
        this.heartbeat_counts.set(
          intent.lease_id,
          (this.heartbeat_counts.get(intent.lease_id) ?? 0) + 1,
        );
        this.completeOperation(
          socket,
          requestId,
          {
            state: "completed",
            result: {
              lease: intent.lease_id,
              expires_at_ms: session.expires_at_ms,
            },
          },
          intent.kind,
          idempotencyKey,
          actor.id,
        );
        return;
      }
      case "resolution_complete": {
        const pending = this.selectResolution(intent.selector);
        this.resolutions.delete(pending.successor.workspace);
        const handoff = this.registerHandoff(
          pending.parent,
          pending.successor,
          actor.id,
        );
        this.emit("resolution_completed", handoff.successor, {
          predecessor: pending.parent.opened.workspace,
        });
        this.acceptOperation(
          socket,
          requestId,
          handoff,
          intent.kind,
          idempotencyKey,
          actor.id,
        );
        return;
      }
      case "repository_warm":
      case "garbage_collect":
      case "maintenance_sweep":
      case "reconcile":
        this.completeOperation(
          socket,
          requestId,
          { state: "completed", result: { ok: true } },
          intent.kind,
          idempotencyKey,
          actor.id,
        );
    }
  }

  private query(socket: Socket, requestId: string, query: Query): void {
    switch (query.kind) {
      case "context": {
        const session = this.select(query.selector);
        this.respond(socket, requestId, {
          state: "completed",
          result: session.opened.compact_context,
        });
        return;
      }
      case "operation": {
        const record = this.operations.get(query.operation_id);
        if (record === undefined) {
          throw new FakeRequestError({
            code: "OPERATION_NOT_FOUND",
            retry: "never",
          });
        }
        this.respond(
          socket,
          requestId,
          { state: "completed", result: record },
          this.operation_query_delay_ms,
        );
        return;
      }
      case "operation_by_key": {
        const operationId = this.operations_by_key.get(
          `${query.actor_id}\u0000${query.idempotency_key}`,
        );
        const record =
          operationId === undefined ? undefined : this.operations.get(operationId);
        if (record === undefined) {
          throw new FakeRequestError({
            code: "OPERATION_NOT_FOUND",
            retry: "never",
          });
        }
        this.respond(socket, requestId, {
          state: "completed",
          result: record,
        });
        return;
      }
      case "events": {
        const selected = this.events
          .filter((event) => event.cursor > query.after_cursor)
          .slice(0, query.limit);
        this.respond(socket, requestId, {
          state: "completed",
          result: {
            events: selected,
            next_cursor: selected.at(-1)?.cursor ?? query.after_cursor,
          },
        });
        return;
      }
      case "session": {
        const session = this.sessions.get(query.session_id);
        if (session === undefined) {
          throw new FakeRequestError({
            code: "SESSION_NOT_FOUND",
            retry: "never",
          });
        }
        this.respond(socket, requestId, {
          state: "completed",
          result: this.describeSession(session),
        });
        return;
      }
      case "doctor":
        this.respond(socket, requestId, {
          state: "completed",
          result: { ok: true, active_leases: this.active_leases },
        });
    }
  }

  private createSession(
    sessionId: string,
    base: string | undefined,
  ): OpenedSessionPayload {
    const opened = this.buildSession(sessionId, base);
    this.sessions.set(sessionId, {
      opened,
      released: false,
      suspended: false,
      expires_at_ms: Date.now() + this.lease_ttl_ms,
      secret_review: false,
    });
    return opened;
  }

  /**
   * A reattach keeps the workspace and its cwd and hands back a fresh lease,
   * exactly as the engine's `reattach_session` transaction does.
   */
  private renewLease(session: FakeSession): OpenedSessionPayload {
    const lease = this.next("lease", ++this.lease_sequence);
    // The daemon omits `lifecycle` when it is `active`, so a reattached
    // session drops the key instead of carrying the word.
    const { lifecycle: _revived, ...context } = session.opened.compact_context;
    const opened: OpenedSessionPayload = {
      ...session.opened,
      lease,
      env: { ...session.opened.env, SHADE_LEASE: lease },
      compact_context: context,
    };
    session.opened = opened;
    session.expires_at_ms = Date.now() + this.lease_ttl_ms;
    return opened;
  }

  /**
   * Waking produces a successor workspace with a new id and a new cwd under
   * the session id that went to sleep, exactly as the engine's
   * `activate_woken_workspace` transaction does.
   */
  private wakeSuspended(session: FakeSession): OpenedSessionPayload {
    const opened = this.buildSession(
      session.opened.session,
      session.opened.compact_context.base_sha,
    );
    session.opened = opened;
    session.expires_at_ms = Date.now() + this.lease_ttl_ms;
    return opened;
  }

  private describeSession(session: FakeSession): SessionStatus {
    const live =
      !session.released && !session.suspended && session.expires_at_ms >= Date.now();
    const lifecycle = session.released
      ? "released"
      : session.suspended
        ? "suspended"
        : live
          ? "active"
          : "dormant";
    const materialized = existsSync(session.opened.cwd);
    return {
      session: session.opened.session,
      lifecycle,
      workspace: session.opened.workspace,
      ...(live
        ? {
            lease: session.opened.lease,
            lease_expires_at_ms: session.expires_at_ms,
          }
        : {}),
      ...(materialized ? { cwd: session.opened.cwd } : {}),
      materialized,
    };
  }

  private buildSession(
    sessionId: string,
    base: string | undefined,
  ): OpenedSessionPayload {
    const workspace = this.next("ws", ++this.workspace_sequence);
    const lease = this.next("lease", ++this.lease_sequence);
    const cwd = join(this.root, "workspaces", workspace);
    mkdirSync(cwd, { recursive: true });
    const baseSha = base ?? this.objectId();
    const context: CompactContext = {
      workspace,
      session: sessionId,
      base_ref: "refs/heads/main",
      base_sha: baseSha,
      head_sha: baseSha,
      changes: { staged: 0, unstaged: 0, untracked: 0 },
      lease: "active",
      dependencies: { state: "ready", providers: [] },
    };
    const opened: OpenedSessionPayload = {
      session: sessionId,
      workspace,
      lease,
      cwd,
      env: {
        SHADE_SESSION: sessionId,
        SHADE_WORKSPACE: workspace,
        SHADE_LEASE: lease,
        SHADE_SOCKET: this.socket,
      },
      compact_context: context,
    };
    return opened;
  }

  private createSuccessor(
    parent: FakeSession,
    actor: string,
  ): PendingHandoffPayload {
    const successor = this.buildSession(parent.opened.session, this.objectId());
    successor.compact_context.dependencies = {
      ...parent.opened.compact_context.dependencies,
    };
    return this.registerHandoff(parent, successor, actor);
  }

  private registerHandoff(
    parent: FakeSession,
    successor: OpenedSessionPayload,
    actor: string,
  ): PendingHandoffPayload {
    const handoffId = this.next("handoff", ++this.handoff_sequence);
    this.handoffs.set(handoffId, {
      actor,
      parent,
      successor,
      adopted: false,
    });
    return {
      handoff_id: handoffId,
      session: successor.session,
      predecessor: parent.opened.workspace,
      successor: successor.workspace,
    };
  }

  /**
   * Resolution selectors carry only the workspace id, matching the engine's
   * `resolve_workspace`: the workspace, not the caller's cwd, is authoritative.
   */
  private selectResolution(selector: WorkspaceSelector): FakeResolution {
    this.selector_checks += 1;
    const pending =
      selector.workspace_id === undefined
        ? [...this.resolutions.values()].find(
            (candidate) => candidate.successor.cwd === selector.cwd,
          )
        : this.resolutions.get(selector.workspace_id);
    if (pending === undefined) {
      throw new FakeRequestError({
        code: "WORKSPACE_NOT_FOUND",
        retry: "never",
      });
    }
    if (pending.parent.released || pending.parent.expires_at_ms < Date.now()) {
      throw new FakeRequestError({
        code: "WORKSPACE_RELEASED",
        retry: "never",
      });
    }
    return pending;
  }

  private select(selector: WorkspaceSelector): FakeSession {
    this.selector_checks += 1;
    if (selector.workspace_id === undefined || selector.cwd === undefined) {
      throw new FakeRequestError({
        code: "SELECTOR_INCOMPLETE",
        retry: "never",
      });
    }
    const session = [...this.sessions.values()].find(
      (candidate) => candidate.opened.workspace === selector.workspace_id,
    );
    if (session === undefined) {
      throw new FakeRequestError({
        code: "WORKSPACE_NOT_FOUND",
        retry: "never",
      });
    }
    if (session.opened.cwd !== selector.cwd) {
      throw new FakeRequestError({
        code: "SELECTOR_MISMATCH",
        retry: "never",
      });
    }
    if (session.released || session.expires_at_ms < Date.now()) {
      throw new FakeRequestError({
        code: "WORKSPACE_RELEASED",
        retry: "never",
      });
    }
    return session;
  }

  private acceptOperation(
    socket: Socket,
    requestId: string,
    result: unknown,
    intentKind: string,
    idempotencyKey: string,
    actorId: string,
  ): void {
    const operationId = this.operationId();
    this.storeOperation(
      operationId,
      { state: "completed", result },
      intentKind,
      idempotencyKey,
      actorId,
    );
    this.respond(socket, requestId, {
      state: "accepted",
      result: { operation_id: operationId },
    });
  }

  private completeOperation(
    socket: Socket,
    requestId: string,
    outcome: OperationOutcome<unknown>,
    intentKind: string,
    idempotencyKey: string,
    actorId: string,
  ): void {
    const operationId = this.operationId();
    this.storeOperation(
      operationId,
      outcome,
      intentKind,
      idempotencyKey,
      actorId,
    );
    this.respond(socket, requestId, outcome);
  }

  private storeOperation(
    operationId: string,
    outcome: OperationOutcome<unknown>,
    intentKind: string,
    idempotencyKey: string,
    actorId: string,
  ): void {
    const timestamp = 1_700_000_000_000 + this.operation_sequence;
    this.operations.set(operationId, {
      id: operationId,
      state: "completed",
      intent_kind: intentKind,
      outcome,
      error: null,
      created_at_ms: timestamp,
      updated_at_ms: timestamp,
    });
    this.operations_by_key.set(`${actorId}\u0000${idempotencyKey}`, operationId);
  }

  private subscribe(socket: Socket, afterCursor: number): void {
    this.subscribers.add(socket);
    for (const event of this.events) {
      if (event.cursor > afterCursor) this.write(socket, event, false);
    }
  }

  private emit(event: string, resource: string, payload: JsonValue): void {
    const cursor = this.events.length + 1;
    const envelope: EventEnvelope = {
      v: PROTOCOL_VERSION,
      cursor,
      event,
      resource,
      payload,
      created_at_ms: 1_700_000_000_000 + cursor,
    };
    this.events.push(envelope);
    for (const socket of this.subscribers) this.write(socket, envelope, false);
  }

  private respond(
    socket: Socket,
    requestId: string,
    outcome: OperationOutcome<unknown>,
    delayMs = 0,
  ): void {
    this.write(
      socket,
      {
        v: PROTOCOL_VERSION,
        request_id: requestId,
        status: "ok",
        outcome,
      },
      true,
      delayMs,
    );
  }

  private respondError(
    socket: Socket,
    requestId: string,
    error: ShadeErrorBody,
  ): void {
    this.write(socket, {
      v: PROTOCOL_VERSION,
      request_id: requestId,
      status: "error",
      error,
    });
  }

  private write(
    socket: Socket,
    value: unknown,
    end = true,
    delayMs = 0,
  ): void {
    const send = () => {
      if (socket.destroyed) return;
      const line = `${JSON.stringify(value)}\n`;
      if (end) socket.end(line);
      else socket.write(line);
    };
    if (delayMs <= 0) {
      send();
      return;
    }
    const timer = setTimeout(() => {
      this.timers.delete(timer);
      send();
    }, delayMs);
    this.timers.add(timer);
  }

  private operationId(): string {
    return this.next("op", ++this.operation_sequence);
  }

  private objectId(): string {
    return this.next("oid", ++this.object_sequence);
  }

  private next(prefix: string, value: number): string {
    return `${prefix}-${value.toString().padStart(4, "0")}`;
  }
}
