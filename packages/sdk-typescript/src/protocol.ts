export const PROTOCOL_VERSION = 1 as const;

// Shade identifiers are deliberately opaque. Callers must never parse their shape.
export type RepositoryId = string;
export type SessionId = string;
export type WorkspaceId = string;
export type LeaseId = string;
export type CheckpointId = string;
export type OperationId = string;
export type ReviewId = string;
export type DependencyLayerId = string;
export type ObjectId = string;
export type HandoffId = string;

export type JsonPrimitive = string | number | boolean | null;
export type JsonValue = JsonPrimitive | JsonValue[] | { [key: string]: JsonValue };

export type ActorKind = "zenith" | "agent" | "cli" | "system";

export interface Actor {
  kind: ActorKind;
  id: string;
}

export type RepositoryLocator =
  | { kind: "local"; path: string }
  | { kind: "remote"; url: string }
  | { kind: "registered"; repository_id: RepositoryId };

export interface OpenSessionInput {
  session_id: string;
  repository: RepositoryLocator;
  base?: string;
  intent?: string;
}

export interface WorkspaceSelector {
  workspace_id?: WorkspaceId;
  cwd?: string;
}

export type ReviewAction = "merge_parent" | "keep" | "discard";

export interface ScriptApproval {
  provider: string;
  package: string;
  version: string;
  integrity: string;
}

export interface DependencyScript {
  approval: ScriptApproval;
  events: string[];
  executed: boolean;
}

export interface DependencyScriptsResult {
  scripts: { script: DependencyScript; allowed: boolean }[];
}

export interface ScriptDecisionResult {
  allowed: boolean;
  refresh_required: boolean;
}

export type Intent =
  | ({ kind: "session_open" } & OpenSessionInput)
  | { kind: "repository_warm"; repository: RepositoryLocator }
  | { kind: "lease_heartbeat"; session_id: SessionId; lease_id: LeaseId }
  | { kind: "workspace_checkpoint"; selector: WorkspaceSelector; reason: string }
  | {
      kind: "workspace_fork";
      selector: WorkspaceSelector;
      child_session_id: string;
      intent?: string;
    }
  | { kind: "workspace_sync"; selector: WorkspaceSelector }
  | {
      kind: "workspace_restore";
      selector: WorkspaceSelector;
      checkpoint_id: CheckpointId;
    }
  | { kind: "dependencies_refresh"; selector: WorkspaceSelector }
  | {
      kind: "dependency_script_decision";
      selector: WorkspaceSelector;
      approval: ScriptApproval;
      allow: boolean;
    }
  | {
      kind: "workspace_publish";
      selector: WorkspaceSelector;
      branch: string;
      message: string;
      push?: boolean;
    }
  | { kind: "resolution_complete"; selector: WorkspaceSelector }
  | { kind: "workspace_release"; selector: WorkspaceSelector }
  | { kind: "review_resolve"; review_id: ReviewId; action: ReviewAction }
  | { kind: "successor_adopt"; handoff_id: HandoffId }
  | { kind: "garbage_collect" }
  | { kind: "maintenance_sweep" }
  | { kind: "reconcile" };

export type Query =
  | { kind: "context"; selector: WorkspaceSelector }
  | { kind: "dependency_scripts"; selector: WorkspaceSelector }
  | { kind: "operation"; operation_id: OperationId }
  | {
      kind: "operation_by_key";
      actor_kind: ActorKind;
      actor_id: string;
      idempotency_key: string;
    }
  | { kind: "events"; after_cursor: number; limit: number }
  | { kind: "doctor" }
  | { kind: "diagnostics"; diagnostics_id: string };

export interface Diagnostic {
  id: string;
  origin: "daemon" | "cli";
  operation: OperationId | null;
  code: string;
  message: string;
  redacted: boolean;
  truncated: boolean;
  created_at_ms: number;
}

export interface CompactChanges {
  staged: number;
  unstaged: number;
  untracked: number;
}

export interface DependencyContext {
  state: string;
  providers?: string[];
  blocked_builds?: string[];
}

export interface CompactContext {
  workspace: WorkspaceId;
  session: SessionId;
  base_ref: string;
  base_sha: ObjectId;
  head_sha: ObjectId;
  remote_sha?: ObjectId;
  changes: CompactChanges;
  lease: string;
  dependencies: DependencyContext;
}

export interface OpenedSessionPayload {
  session: SessionId;
  workspace: WorkspaceId;
  lease: LeaseId;
  cwd: string;
  env: Record<string, string>;
  compact_context: CompactContext;
}

export interface PendingHandoffPayload {
  handoff_id: HandoffId;
  session: SessionId;
  predecessor: WorkspaceId;
  successor: WorkspaceId;
}

export interface SecretKeyPreview {
  key: string;
  result: string;
}

export interface SecretFilePreview {
  path: string;
  file_result: "parent" | "child" | "unchanged" | "removed" | "conflict";
  keys?: SecretKeyPreview[];
}

export interface ReviewRequired {
  review_id: ReviewId;
  kind: string;
  files: SecretFilePreview[];
}

export interface ConflictOutcome {
  operation_id: OperationId;
  workspace: WorkspaceId;
  cwd: string;
  paths: string[];
}

export type CompletedOutcome<T> = { state: "completed"; result: T };
export type AcceptedOutcome = {
  state: "accepted";
  result: { operation_id: OperationId };
};
export type ReviewRequiredOutcome = {
  state: "review_required";
  result: ReviewRequired;
};
export type ConflictResult = { state: "conflict"; result: ConflictOutcome };

export type OperationOutcome<T> =
  | CompletedOutcome<T>
  | AcceptedOutcome
  | ReviewRequiredOutcome
  | ConflictResult;

export type TerminalOutcome<T> = Exclude<OperationOutcome<T>, AcceptedOutcome>;

export interface ShadeErrorBody {
  code: string;
  retry: string;
  operation?: OperationId;
  next?: string;
  diagnostics_id?: string;
}

export type OperationState = "running" | "completed" | "failed";

export interface OperationRecord<T = unknown> {
  id: OperationId;
  state: OperationState;
  intent_kind: string;
  outcome: OperationOutcome<T> | null;
  error: ShadeErrorBody | null;
  created_at_ms: number;
  updated_at_ms: number;
}

export interface ExecuteRequest {
  type: "execute";
  v: typeof PROTOCOL_VERSION;
  request_id: string;
  idempotency_key: string;
  actor: Actor;
  intent: Intent;
}

export interface QueryRequest {
  type: "query";
  v: typeof PROTOCOL_VERSION;
  request_id: string;
  query: Query;
}

export interface SubscribeRequest {
  type: "subscribe";
  v: typeof PROTOCOL_VERSION;
  request_id: string;
  after_cursor: number;
}

export type WireRequest = ExecuteRequest | QueryRequest | SubscribeRequest;

export type WireResponse<T = JsonValue> =
  | {
      v: typeof PROTOCOL_VERSION;
      request_id: string;
      status: "ok";
      outcome: OperationOutcome<T>;
    }
  | {
      v: typeof PROTOCOL_VERSION;
      request_id: string;
      status: "error";
      error: ShadeErrorBody;
    };

export interface EventEnvelope<T extends JsonValue = JsonValue> {
  v: typeof PROTOCOL_VERSION;
  cursor: number;
  event: string;
  resource: string;
  payload: T;
  created_at_ms: number;
}

export interface CheckpointResult {
  checkpoint_id: CheckpointId;
  head_sha: ObjectId;
  index_tree: ObjectId;
  working_tree: ObjectId;
}

export interface PublishResult {
  branch: string;
  commit: ObjectId;
  tree: ObjectId;
  previous_remote: ObjectId | null;
  pushed: boolean;
}

export interface ReleaseResult {
  session: SessionId;
  workspace: WorkspaceId;
  checkpoint_id: CheckpointId | null;
  released: boolean;
}

export type ReviewResolutionResult =
  | { review: ReviewId; resolution: "discarded"; released: WorkspaceId }
  | { review: ReviewId; resolution: "kept"; workspace: WorkspaceId };

export interface HeartbeatResult {
  lease: LeaseId;
  expires_at_ms: number;
}
