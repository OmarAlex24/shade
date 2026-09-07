import { randomUUID } from "node:crypto";

import { ShadeError, ShadeTimeoutError, asShadeError } from "./errors.ts";
import {
  PROTOCOL_VERSION,
  type Actor,
  type CheckpointId,
  type CheckpointResult,
  type CompactContext,
  type ConflictOutcome,
  type DependencyScriptsResult,
  type Diagnostic,
  type EventEnvelope,
  type HeartbeatResult,
  type Intent,
  type LeaseId,
  type OpenedSessionPayload,
  type OpenSessionInput,
  type OperationId,
  type OperationOutcome,
  type OperationRecord,
  type PendingHandoffPayload,
  type PublishResult,
  type Query,
  type ReleaseResult,
  type ReviewAction,
  type ReviewId,
  type ReviewResolutionResult,
  type SessionId,
  type ScriptApproval,
  type ScriptDecisionResult,
  type TerminalOutcome,
  type WireResponse,
  type WorkspaceId,
} from "./protocol.ts";
import { NdjsonUnixTransport } from "./transport.ts";

const retireHandle = Symbol("shade.retireHandle");
const refreshHandle = Symbol("shade.refreshHandle");

export interface ShadeClientOptions {
  socket: string;
  actor: Actor;
  timeout_ms?: number;
  operation_poll_ms?: number;
  event_reconnect_ms?: number;
  heartbeat_interval_ms?: number;
  max_frame_bytes?: number;
}

export interface CallOptions {
  timeout_ms?: number;
}

export interface MutationOptions extends CallOptions {
  idempotency_key?: string;
}

export interface EventOptions {
  signal?: AbortSignal;
  reconnect_ms?: number;
}

export interface ForkInput {
  child_session_id: string;
  intent?: string;
}

export interface PublishInput {
  branch: string;
  message: string;
  push?: boolean;
}

export interface SessionsApi {
  open(input: OpenSessionInput, options?: MutationOptions): Promise<ShadeSession>;
}

export interface OperationsApi {
  get<T = unknown>(
    operation_id: OperationId,
    options?: CallOptions,
  ): Promise<OperationRecord<T>>;
  wait<T = unknown>(
    operation_id: OperationId,
    options?: CallOptions,
  ): Promise<TerminalOutcome<T>>;
  getByKey<T = unknown>(
    idempotency_key: string,
    options?: CallOptions,
  ): Promise<OperationRecord<T>>;
  waitByKey<T = unknown>(
    idempotency_key: string,
    options?: CallOptions,
  ): Promise<TerminalOutcome<T>>;
}

export interface ReviewsApi {
  resolve(
    review_id: ReviewId,
    action: ReviewAction,
    options?: MutationOptions,
  ): Promise<TerminalOutcome<ReviewResolutionResult | ShadeSession>>;
}

interface SessionBridge {
  queryAndWait<T>(
    query: Query,
    options?: CallOptions,
  ): Promise<TerminalOutcome<T>>;
  executeAndWait<T>(
    intent: Intent,
    options?: MutationOptions,
  ): Promise<TerminalOutcome<T>>;
  session(payload: OpenedSessionPayload): ShadeSession;
  retire(session: ShadeSession): void;
  heartbeat(session_id: SessionId, lease_id: LeaseId): Promise<HeartbeatResult>;
  heartbeat_interval_ms: number;
}

export class ShadeClient {
  readonly sessions: SessionsApi;
  readonly operations: OperationsApi;
  readonly reviews: ReviewsApi;

  readonly actor: Actor;
  readonly timeout_ms: number;
  readonly operation_poll_ms: number;
  readonly event_reconnect_ms: number;
  readonly heartbeat_interval_ms: number;

  private readonly transport: NdjsonUnixTransport;
  private readonly sessionBridge: SessionBridge;
  private readonly liveSessions = new Map<SessionId, ShadeSession>();

  constructor(options: ShadeClientOptions) {
    this.actor = Object.freeze({ ...options.actor });
    this.timeout_ms = options.timeout_ms ?? 30_000;
    this.operation_poll_ms = options.operation_poll_ms ?? 25;
    this.event_reconnect_ms = options.event_reconnect_ms ?? 100;
    this.heartbeat_interval_ms = options.heartbeat_interval_ms ?? 30_000;
    if (!Number.isSafeInteger(this.heartbeat_interval_ms) || this.heartbeat_interval_ms <= 0) {
      throw new ShadeError({
        code: "CLIENT_INVALID_HEARTBEAT_INTERVAL",
        retry: "never",
      });
    }
    this.transport = new NdjsonUnixTransport({
      socket: options.socket,
      ...(options.max_frame_bytes === undefined
        ? {}
        : { max_frame_bytes: options.max_frame_bytes }),
    });
    this.sessionBridge = {
      queryAndWait: <T>(query: Query, call?: CallOptions) =>
        this.queryAndWait<T>(query, call),
      executeAndWait: <T>(intent: Intent, call?: MutationOptions) =>
        this.executeMutationAndWait<T>(intent, call),
      session: (payload: OpenedSessionPayload) => this.hydrateSession(payload),
      retire: (session: ShadeSession) => this.retireSession(session),
      heartbeat: (session_id: SessionId, lease_id: LeaseId) =>
        this.heartbeat(session_id, lease_id),
      heartbeat_interval_ms: this.heartbeat_interval_ms,
    };

    this.sessions = Object.freeze({
      open: (input: OpenSessionInput, call?: MutationOptions) =>
        this.openSession(input, call),
    });
    this.operations = Object.freeze({
      get: <T = unknown>(id: OperationId, call?: CallOptions) =>
        this.getKnownOperation<T>(id, call),
      wait: <T = unknown>(id: OperationId, call?: CallOptions) =>
        this.waitKnownOperation<T>(id, call),
      getByKey: <T = unknown>(key: string, call?: CallOptions) =>
        this.getOperationByKey<T>(key, call),
      waitByKey: <T = unknown>(key: string, call?: CallOptions) =>
        this.waitOperationByKey<T>(key, call),
    });
    this.reviews = Object.freeze({
      resolve: (review_id: ReviewId, action: ReviewAction, call?: MutationOptions) =>
        this.resolveReview(review_id, action, call),
    });
  }

  async diagnostics(diagnostics_id: string, options?: CallOptions): Promise<Diagnostic> {
    return completed(
      await this.queryAndWait<Diagnostic>({ kind: "diagnostics", diagnostics_id }, options),
      "DIAGNOSTIC_INCOMPLETE",
    );
  }

  async *events(
    afterCursor = 0,
    options: EventOptions = {},
  ): AsyncGenerator<EventEnvelope, void, void> {
    if (!Number.isSafeInteger(afterCursor) || afterCursor < 0) {
      throw new ShadeError({ code: "CLIENT_INVALID_CURSOR", retry: "never" });
    }
    let cursor = afterCursor;
    const reconnectMs = options.reconnect_ms ?? this.event_reconnect_ms;

    while (!options.signal?.aborted) {
      const request = {
        type: "subscribe" as const,
        v: PROTOCOL_VERSION,
        request_id: randomUUID(),
        after_cursor: cursor,
      };

      try {
        for await (const raw of this.transport.stream(request, options.signal)) {
          const event = parseEvent(raw);
          if (event.cursor <= cursor) continue;
          cursor = event.cursor;
          yield event;
        }
      } catch (error) {
        if (options.signal?.aborted) return;
        const shadeError = asShadeError(error);
        if (shadeError.retry === "never") throw shadeError;
      }

      if (options.signal?.aborted) return;
      await delay(reconnectMs, options.signal);
    }
  }

  private async queryAndWait<T>(
    query: Query,
    options?: CallOptions,
  ): Promise<TerminalOutcome<T>> {
    const deadline = this.deadline(options);
    const outcome = await this.query<T>(query, deadline);
    return await this.settle(outcome, deadline);
  }

  private async executeMutationAndWait<T>(
    intent: Intent,
    options?: MutationOptions,
  ): Promise<TerminalOutcome<T>> {
    return await this.executeAndWait<T>(
      intent,
      this.deadline(options),
      this.idempotencyKey(options),
    );
  }

  private async openSession(
    input: OpenSessionInput,
    options?: MutationOptions,
  ): Promise<ShadeSession> {
    const outcome = await this.executeAndWait<OpenedSessionPayload>(
      { kind: "session_open", ...input },
      this.deadline(options),
      this.idempotencyKey(options),
    );
    const payload = completed(outcome, "SESSION_OPEN_INCOMPLETE");
    return this.sessionBridge.session(payload);
  }

  private hydrateSession(payload: OpenedSessionPayload): ShadeSession {
    if (!isOpenedSessionPayload(payload)) {
      throw protocolError(
        "CLIENT_INVALID_OPENED_SESSION",
        "Completed successor is not an OpenedSession payload",
      );
    }
    const existing = this.liveSessions.get(payload.session);
    if (
      existing !== undefined &&
      existing.workspace === payload.workspace &&
      existing.lease === payload.lease
    ) {
      existing[refreshHandle](payload);
      return existing;
    }
    if (existing !== undefined) this.retireSession(existing);
    const session = new ShadeSession(this.sessionBridge, payload);
    this.liveSessions.set(session.session, session);
    return session;
  }

  private retireSession(session: ShadeSession): void {
    if (this.liveSessions.get(session.session) === session) {
      this.liveSessions.delete(session.session);
    }
    session[retireHandle]();
  }

  private retireWorkspace(workspace: WorkspaceId): void {
    for (const session of this.liveSessions.values()) {
      if (session.workspace === workspace) {
        this.retireSession(session);
        return;
      }
    }
  }

  private async heartbeat(
    sessionId: SessionId,
    leaseId: LeaseId,
  ): Promise<HeartbeatResult> {
    const outcome = await this.executeAndWait<HeartbeatResult>(
      { kind: "lease_heartbeat", session_id: sessionId, lease_id: leaseId },
      this.deadline(),
      randomUUID(),
    );
    return completed(outcome, "LEASE_HEARTBEAT_INCOMPLETE");
  }

  private async resolveReview(
    reviewId: ReviewId,
    action: ReviewAction,
    options?: MutationOptions,
  ): Promise<TerminalOutcome<ReviewResolutionResult | ShadeSession>> {
    const outcome = await this.executeAndWait<
      ReviewResolutionResult | OpenedSessionPayload
    >(
      { kind: "review_resolve", review_id: reviewId, action },
      this.deadline(options),
      this.idempotencyKey(options),
    );
    if (outcome.state !== "completed") return outcome;
    if (isOpenedSessionPayload(outcome.result)) {
      return { state: "completed", result: this.hydrateSession(outcome.result) };
    }
    if ("released" in outcome.result) {
      this.retireWorkspace(outcome.result.released);
    } else {
      this.retireWorkspace(outcome.result.workspace);
    }
    return outcome as TerminalOutcome<ReviewResolutionResult | ShadeSession>;
  }

  private async getKnownOperation<T>(
    operationId: OperationId,
    options?: CallOptions,
  ): Promise<OperationRecord<T>> {
    try {
      return await this.queryOperationRecord<T>(
        { kind: "operation", operation_id: operationId },
        this.deadline(options),
        operationId,
      );
    } catch (error) {
      throw attachOperation(error, operationId);
    }
  }

  private async getOperationByKey<T>(
    idempotencyKey: string,
    options?: CallOptions,
  ): Promise<OperationRecord<T>> {
    this.validateIdempotencyKey(idempotencyKey);
    return await this.queryOperationRecord<T>(
      {
        kind: "operation_by_key",
        actor_kind: this.actor.kind,
        actor_id: this.actor.id,
        idempotency_key: idempotencyKey,
      },
      this.deadline(options),
    );
  }

  private async waitKnownOperation<T>(
    operationId: OperationId,
    options?: CallOptions,
  ): Promise<TerminalOutcome<T>> {
    try {
      return await this.waitOperation<T>(operationId, this.deadline(options));
    } catch (error) {
      throw attachOperation(error, operationId);
    }
  }

  private async waitOperationByKey<T>(
    idempotencyKey: string,
    options?: CallOptions,
  ): Promise<TerminalOutcome<T>> {
    this.validateIdempotencyKey(idempotencyKey);
    const deadline = this.deadline(options);
    try {
      const record = await this.awaitOperationByKeyRecord<T>(
        idempotencyKey,
        deadline,
      );
      return await this.waitOperationRecord(record, deadline);
    } catch (error) {
      if (error instanceof ShadeTimeoutError) {
        throw new ShadeTimeoutError(error.operation_id, {
          cause: error,
          idempotency_key: idempotencyKey,
        });
      }
      throw error;
    }
  }

  private async executeAndWait<T>(
    intent: Intent,
    deadline: number,
    idempotencyKey: string,
  ): Promise<TerminalOutcome<T>> {
    const outcome = await this.execute<T>(intent, deadline, idempotencyKey);
    return await this.settle(outcome, deadline, idempotencyKey);
  }

  private async settle<T>(
    outcome: OperationOutcome<T>,
    deadline: number,
    idempotencyKey?: string,
  ): Promise<TerminalOutcome<T>> {
    if (outcome.state !== "accepted") {
      return await this.adoptPendingHandoff(outcome, deadline);
    }
    const operationId = outcome.result.operation_id;
    try {
      return await this.waitOperation<T>(operationId, deadline);
    } catch (error) {
      if (error instanceof ShadeTimeoutError) {
        throw new ShadeTimeoutError(operationId, {
          cause: error,
          ...(idempotencyKey === undefined
            ? {}
            : { idempotency_key: idempotencyKey }),
        });
      }
      if (error instanceof ShadeError && error.operation === undefined) {
        throw new ShadeError(
          {
            ...error.toJSON(),
            operation: operationId,
          },
          error.message,
          { cause: error },
        );
      }
      throw error;
    }
  }

  private async waitOperation<T>(
    operationId: OperationId,
    deadline: number,
  ): Promise<TerminalOutcome<T>> {
    const record = await this.queryOperationRecord<T>(
      { kind: "operation", operation_id: operationId },
      deadline,
      operationId,
    );
    return await this.waitOperationRecord(record, deadline);
  }

  private async waitOperationRecord<T>(
    initial: OperationRecord<T>,
    deadline: number,
  ): Promise<TerminalOutcome<T>> {
    let record = initial;
    for (;;) {
      if (record.state === "completed") {
        if (record.outcome === null || record.outcome.state === "accepted") {
          throw protocolError(
            "CLIENT_INVALID_OPERATION_RECORD",
            "Completed operation has no terminal outcome",
          );
        }
        return await this.adoptPendingHandoff(record.outcome, deadline);
      }
      if (record.state === "failed") {
        if (record.error === null) {
          throw protocolError(
            "CLIENT_INVALID_OPERATION_RECORD",
            "Failed operation has no error",
          );
        }
        throw attachOperation(new ShadeError(record.error), record.id);
      }

      await delay(Math.min(this.operation_poll_ms, remaining(deadline)));
      record = await this.queryOperationRecord<T>(
        { kind: "operation", operation_id: record.id },
        deadline,
        record.id,
      );
    }
  }

  private async adoptPendingHandoff<T>(
    outcome: TerminalOutcome<T>,
    deadline: number,
  ): Promise<TerminalOutcome<T>> {
    if (outcome.state !== "completed" || !isPendingHandoffPayload(outcome.result)) {
      return outcome;
    }
    return await this.executeAndWait<T>(
      { kind: "successor_adopt", handoff_id: outcome.result.handoff_id },
      deadline,
      `handoff:${outcome.result.handoff_id}`,
    );
  }

  private async queryOperationRecord<T>(
    query: Extract<Query, { kind: "operation" | "operation_by_key" }>,
    deadline: number,
    expectedId?: OperationId,
  ): Promise<OperationRecord<T>> {
    const envelope = await this.query<OperationRecord<T>>(query, deadline);
    if (envelope.state !== "completed") {
      throw protocolError(
        "CLIENT_INVALID_OPERATION_QUERY",
        "Operation query did not return a completed record envelope",
      );
    }
    const record = parseOperationRecord<T>(envelope.result);
    if (expectedId !== undefined && record.id !== expectedId) {
      throw protocolError(
        "CLIENT_OPERATION_MISMATCH",
        "Operation record id mismatch",
      );
    }
    return record;
  }

  private async execute<T>(
    intent: Intent,
    deadline: number,
    idempotencyKey: string,
  ): Promise<OperationOutcome<T>> {
    const requestId = randomUUID();
    try {
      const raw = await this.transport.request(
        {
          type: "execute",
          v: PROTOCOL_VERSION,
          request_id: requestId,
          idempotency_key: idempotencyKey,
          actor: this.actor,
          intent,
        },
        remaining(deadline),
      );
      return responseOutcome<T>(raw, requestId);
    } catch (error) {
      if (!(error instanceof ShadeTimeoutError)) throw error;
      const operation = await this.lookupTimedOutMutation(idempotencyKey);
      throw new ShadeTimeoutError(operation, {
        cause: error,
        idempotency_key: idempotencyKey,
      });
    }
  }

  private async lookupTimedOutMutation(
    idempotencyKey: string,
  ): Promise<OperationId | undefined> {
    const deadline = Date.now() + Math.min(this.timeout_ms, 1_000);
    try {
      const record = await this.awaitOperationByKeyRecord<unknown>(
        idempotencyKey,
        deadline,
      );
      return record.id;
    } catch {
      return undefined;
    }
  }

  private async awaitOperationByKeyRecord<T>(
    idempotencyKey: string,
    deadline: number,
  ): Promise<OperationRecord<T>> {
    for (;;) {
      try {
        return await this.queryOperationRecord<T>(
          {
            kind: "operation_by_key",
            actor_kind: this.actor.kind,
            actor_id: this.actor.id,
            idempotency_key: idempotencyKey,
          },
          deadline,
        );
      } catch (error) {
        if (
          !(error instanceof ShadeError) ||
          error.code !== "OPERATION_NOT_FOUND" ||
          Date.now() >= deadline
        ) {
          throw error;
        }
      }
      await delay(Math.min(this.operation_poll_ms, remaining(deadline)));
    }
  }

  private async query<T>(
    query: Query,
    deadline: number,
  ): Promise<OperationOutcome<T>> {
    const requestId = randomUUID();
    const raw = await this.transport.request(
      {
        type: "query",
        v: PROTOCOL_VERSION,
        request_id: requestId,
        query,
      },
      remaining(deadline),
    );
    return responseOutcome<T>(raw, requestId);
  }

  private deadline(options?: CallOptions): number {
    return Date.now() + (options?.timeout_ms ?? this.timeout_ms);
  }

  private idempotencyKey(options?: MutationOptions): string {
    const value = options?.idempotency_key ?? randomUUID();
    this.validateIdempotencyKey(value);
    return value;
  }

  private validateIdempotencyKey(value: string): void {
    if (value.length === 0 || value.length > 256) {
      throw new ShadeError({
        code: "CLIENT_INVALID_IDEMPOTENCY_KEY",
        retry: "never",
      });
    }
  }
}

export class ShadeSession {
  readonly session: SessionId;
  readonly workspace: WorkspaceId;
  readonly lease: LeaseId;
  readonly cwd: string;
  readonly env: Readonly<Record<string, string>>;

  private compactContext: CompactContext;
  private readonly bridge: SessionBridge;
  private heartbeatTimer: ReturnType<typeof setInterval> | undefined;
  private heartbeatInFlight = false;
  private retired = false;

  /** @internal Sessions are created by ShadeClient.sessions.open or session.fork. */
  constructor(bridge: SessionBridge, payload: OpenedSessionPayload) {
    this.bridge = bridge;
    this.session = payload.session;
    this.workspace = payload.workspace;
    this.lease = payload.lease;
    this.cwd = payload.cwd;
    this.env = Object.freeze({ ...payload.env });
    this.compactContext = payload.compact_context;
    this.startHeartbeat();
  }

  get compact_context(): CompactContext {
    return this.compactContext;
  }

  async context(options?: CallOptions): Promise<CompactContext> {
    this.assertLive();
    const outcome = await this.bridge.queryAndWait<CompactContext>(
      { kind: "context", selector: this.selector() },
      options,
    );
    this.compactContext = completed(outcome, "CONTEXT_INCOMPLETE");
    return this.compactContext;
  }

  async checkpoint(
    reason: string,
    options?: MutationOptions,
  ): Promise<TerminalOutcome<CheckpointResult>> {
    this.assertLive();
    return await this.bridge.executeAndWait<CheckpointResult>(
      { kind: "workspace_checkpoint", selector: this.selector(), reason },
      options,
    );
  }

  async fork(input: ForkInput, options?: MutationOptions): Promise<ShadeSession> {
    this.assertLive();
    const outcome = await this.bridge.executeAndWait<OpenedSessionPayload>(
      {
        kind: "workspace_fork",
        selector: this.selector(),
        child_session_id: input.child_session_id,
        ...(input.intent === undefined ? {} : { intent: input.intent }),
      },
      options,
    );
    return this.bridge.session(completed(outcome, "SESSION_FORK_INCOMPLETE"));
  }

  async sync(options?: MutationOptions): Promise<TerminalOutcome<ShadeSession>> {
    this.assertLive();
    const outcome = await this.bridge.executeAndWait<OpenedSessionPayload>(
      { kind: "workspace_sync", selector: this.selector() },
      options,
    );
    return this.successor(outcome);
  }

  async restore(
    checkpoint_id: CheckpointId,
    options?: MutationOptions,
  ): Promise<TerminalOutcome<ShadeSession>> {
    this.assertLive();
    const outcome = await this.bridge.executeAndWait<OpenedSessionPayload>(
      {
        kind: "workspace_restore",
        selector: this.selector(),
        checkpoint_id,
      },
      options,
    );
    return this.successor(outcome);
  }

  async refreshDependencies(
    options?: MutationOptions,
  ): Promise<TerminalOutcome<ShadeSession>> {
    this.assertLive();
    const outcome = await this.bridge.executeAndWait<OpenedSessionPayload>(
      { kind: "dependencies_refresh", selector: this.selector() },
      options,
    );
    return this.successor(outcome);
  }

  async dependencyScripts(options?: CallOptions): Promise<DependencyScriptsResult> {
    this.assertLive();
    return completed(
      await this.bridge.queryAndWait<DependencyScriptsResult>(
        { kind: "dependency_scripts", selector: this.selector() },
        options,
      ),
      "DEPENDENCY_SCRIPTS_INCOMPLETE",
    );
  }

  async approveScript(
    approval: ScriptApproval,
    options?: MutationOptions,
  ): Promise<TerminalOutcome<ScriptDecisionResult>> {
    this.assertLive();
    return this.bridge.executeAndWait<ScriptDecisionResult>(
      { kind: "dependency_script_decision", selector: this.selector(), approval, allow: true },
      options,
    );
  }

  async revokeScript(
    approval: ScriptApproval,
    options?: MutationOptions,
  ): Promise<TerminalOutcome<ScriptDecisionResult>> {
    this.assertLive();
    return this.bridge.executeAndWait<ScriptDecisionResult>(
      { kind: "dependency_script_decision", selector: this.selector(), approval, allow: false },
      options,
    );
  }

  async publish(
    input: PublishInput,
    options?: MutationOptions,
  ): Promise<TerminalOutcome<PublishResult>> {
    this.assertLive();
    return await this.bridge.executeAndWait<PublishResult>(
      {
        kind: "workspace_publish",
        selector: this.selector(),
        branch: input.branch,
        message: input.message,
        ...(input.push === undefined ? {} : { push: input.push }),
      },
      options,
    );
  }

  /**
   * Finish a `conflict` outcome. Fix the conflict inside the returned
   * resolution workspace first: the daemon publishes from that workspace and
   * hands the parent session a successor, exactly as `sync` does.
   */
  async resolve(
    conflict: ConflictOutcome | { workspace: WorkspaceId },
    options?: MutationOptions,
  ): Promise<TerminalOutcome<ShadeSession>> {
    this.assertLive();
    const outcome = await this.bridge.executeAndWait<OpenedSessionPayload>(
      {
        kind: "resolution_complete",
        selector: { workspace_id: conflict.workspace },
      },
      options,
    );
    return this.successor(outcome);
  }

  async release(options?: MutationOptions): Promise<TerminalOutcome<ReleaseResult>> {
    this.assertLive();
    const outcome = await this.bridge.executeAndWait<ReleaseResult>(
      { kind: "workspace_release", selector: this.selector() },
      options,
    );
    if (outcome.state === "completed") this.bridge.retire(this);
    return outcome;
  }

  private selector() {
    return { workspace_id: this.workspace, cwd: this.cwd };
  }

  private successor(
    outcome: TerminalOutcome<OpenedSessionPayload>,
  ): TerminalOutcome<ShadeSession> {
    if (outcome.state !== "completed") return outcome;
    return { state: "completed", result: this.bridge.session(outcome.result) };
  }

  private startHeartbeat(): void {
    this.heartbeatTimer = setInterval(() => {
      void this.sendHeartbeat();
    }, this.bridge.heartbeat_interval_ms);
    const timer = this.heartbeatTimer;
    if (
      typeof timer === "object" &&
      timer !== null &&
      "unref" in timer &&
      typeof timer.unref === "function"
    ) {
      timer.unref();
    }
  }

  private async sendHeartbeat(): Promise<void> {
    if (this.retired || this.heartbeatInFlight) return;
    this.heartbeatInFlight = true;
    try {
      await this.bridge.heartbeat(this.session, this.lease);
    } catch (error) {
      const code = asShadeError(error).code;
      if (
        code === "LEASE_EXPIRED" ||
        code === "LEASE_FENCED" ||
        code === "WORKSPACE_RELEASED" ||
        code === "SESSION_ALREADY_RELEASED"
      ) {
        this.bridge.retire(this);
      }
    } finally {
      this.heartbeatInFlight = false;
    }
  }

  private assertLive(): void {
    if (this.retired) {
      throw new ShadeError({
        code: "SESSION_HANDLE_RETIRED",
        retry: "never",
        next: "use the completed successor session",
      });
    }
  }

  [refreshHandle](payload: OpenedSessionPayload): void {
    this.compactContext = payload.compact_context;
  }

  [retireHandle](): void {
    if (this.retired) return;
    this.retired = true;
    if (this.heartbeatTimer !== undefined) {
      clearInterval(this.heartbeatTimer);
      this.heartbeatTimer = undefined;
    }
  }
}

function responseOutcome<T>(
  raw: unknown,
  requestId: string,
): OperationOutcome<T> {
  if (!isObject(raw)) {
    throw protocolError("CLIENT_INVALID_RESPONSE", "Response is not an object");
  }
  const response = raw as unknown as WireResponse<T>;
  if (response.v !== PROTOCOL_VERSION) {
    throw protocolError("CLIENT_PROTOCOL_VERSION", "Protocol version mismatch");
  }
  if (response.request_id !== requestId) {
    throw protocolError("CLIENT_REQUEST_MISMATCH", "Response request_id mismatch");
  }
  if (response.status === "error") {
    if (!isShadeErrorBody(response.error)) {
      throw protocolError("CLIENT_INVALID_RESPONSE", "Error body is invalid");
    }
    throw new ShadeError(response.error);
  }
  if (response.status !== "ok" || !isOutcome(response.outcome)) {
    throw protocolError("CLIENT_INVALID_RESPONSE", "Response outcome is invalid");
  }
  return response.outcome;
}

function parseEvent(raw: unknown): EventEnvelope {
  if (!isObject(raw)) {
    throw protocolError("CLIENT_INVALID_EVENT", "Event is not an object");
  }
  if (raw.status === "error" && isObject(raw.error)) {
    if (!isShadeErrorBody(raw.error)) {
      throw protocolError("CLIENT_INVALID_EVENT", "Stream error body is invalid");
    }
    throw new ShadeError(raw.error);
  }
  if (
    raw.v !== PROTOCOL_VERSION ||
    typeof raw.cursor !== "number" ||
    !Number.isSafeInteger(raw.cursor) ||
    typeof raw.event !== "string" ||
    typeof raw.resource !== "string" ||
    typeof raw.created_at_ms !== "number"
  ) {
    throw protocolError("CLIENT_INVALID_EVENT", "Event envelope is invalid");
  }
  return raw as unknown as EventEnvelope;
}

function completed<T>(outcome: TerminalOutcome<T>, code: string): T {
  if (outcome.state === "completed") return outcome.result;
  const operation =
    outcome.state === "conflict" ? outcome.result.operation_id : undefined;
  throw new ShadeError({
    code,
    retry: "never",
    ...(operation === undefined ? {} : { operation }),
    next:
      outcome.state === "review_required"
        ? `reviews.resolve(${outcome.result.review_id})`
        : "inspect_outcome",
  });
}

function isOutcome(value: unknown): value is OperationOutcome<never> {
  if (!isObject(value) || !("result" in value)) return false;
  if (value.state === "completed") return true;
  if (!isObject(value.result)) return false;
  if (value.state === "accepted") {
    return typeof value.result.operation_id === "string";
  }
  if (value.state === "review_required") {
    return typeof value.result.review_id === "string";
  }
  if (value.state === "conflict") {
    return (
      typeof value.result.operation_id === "string" &&
      typeof value.result.workspace === "string" &&
      typeof value.result.cwd === "string" &&
      Array.isArray(value.result.paths)
    );
  }
  return false;
}

function parseOperationRecord<T>(value: unknown): OperationRecord<T> {
  if (
    !isObject(value) ||
    typeof value.id !== "string" ||
    (value.state !== "running" &&
      value.state !== "completed" &&
      value.state !== "failed") ||
    typeof value.intent_kind !== "string" ||
    typeof value.created_at_ms !== "number" ||
    typeof value.updated_at_ms !== "number" ||
    !("outcome" in value) ||
    !("error" in value) ||
    (value.outcome !== null && !isOutcome(value.outcome)) ||
    (value.error !== null && !isShadeErrorBody(value.error))
  ) {
    throw protocolError(
      "CLIENT_INVALID_OPERATION_RECORD",
      "Operation query returned an invalid record",
    );
  }
  return value as unknown as OperationRecord<T>;
}

function isOpenedSessionPayload(value: unknown): value is OpenedSessionPayload {
  if (
    !isObject(value) ||
    typeof value.session !== "string" ||
    typeof value.workspace !== "string" ||
    typeof value.lease !== "string" ||
    typeof value.cwd !== "string" ||
    !isStringRecord(value.env) ||
    !isObject(value.compact_context)
  ) {
    return false;
  }
  const context = value.compact_context;
  return (
    context.session === value.session &&
    context.workspace === value.workspace &&
    typeof context.base_ref === "string" &&
    typeof context.base_sha === "string" &&
    typeof context.head_sha === "string" &&
    isObject(context.changes) &&
    typeof context.lease === "string" &&
    isObject(context.dependencies)
  );
}

function isPendingHandoffPayload(value: unknown): value is PendingHandoffPayload {
  return (
    isObject(value) &&
    typeof value.handoff_id === "string" &&
    typeof value.session === "string" &&
    typeof value.predecessor === "string" &&
    typeof value.successor === "string"
  );
}

function isStringRecord(value: unknown): value is Record<string, string> {
  return (
    isObject(value) &&
    Object.values(value).every((entry) => typeof entry === "string")
  );
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function protocolError(code: string, message: string): ShadeError {
  return new ShadeError({ code, retry: "never" }, message);
}

function isShadeErrorBody(value: unknown): value is import("./protocol.ts").ShadeErrorBody {
  return (
    isObject(value) &&
    typeof value.code === "string" &&
    typeof value.retry === "string" &&
    (value.operation === undefined || typeof value.operation === "string") &&
    (value.next === undefined || typeof value.next === "string") &&
    (value.diagnostics_id === undefined ||
      typeof value.diagnostics_id === "string")
  );
}

function attachOperation(error: unknown, operation: OperationId): ShadeError {
  if (error instanceof ShadeTimeoutError) {
    return new ShadeTimeoutError(operation, { cause: error });
  }
  const shadeError = asShadeError(error);
  if (shadeError.operation !== undefined) return shadeError;
  return new ShadeError(
    { ...shadeError.toJSON(), operation },
    shadeError.message,
    { cause: shadeError },
  );
}

function remaining(deadline: number): number {
  const value = deadline - Date.now();
  if (value <= 0) throw new ShadeTimeoutError();
  return value;
}

async function delay(ms: number, signal?: AbortSignal): Promise<void> {
  if (ms <= 0 || signal?.aborted) return;
  await new Promise<void>((resolve) => {
    const timer = setTimeout(done, ms);
    const abort = () => done();
    function done() {
      clearTimeout(timer);
      signal?.removeEventListener("abort", abort);
      resolve();
    }
    signal?.addEventListener("abort", abort, { once: true });
  });
}
