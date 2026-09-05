import type { OperationId, ShadeErrorBody } from "./protocol.ts";

export class ShadeError extends Error implements ShadeErrorBody {
  readonly code: string;
  readonly retry: string;
  readonly operation?: OperationId;
  readonly next?: string;
  readonly diagnostics_id?: string;

  constructor(body: ShadeErrorBody, message = body.code, options?: ErrorOptions) {
    super(message, options);
    this.name = "ShadeError";
    this.code = body.code;
    this.retry = body.retry;
    if (body.operation !== undefined) this.operation = body.operation;
    if (body.next !== undefined) this.next = body.next;
    if (body.diagnostics_id !== undefined) this.diagnostics_id = body.diagnostics_id;
  }

  get operation_id(): OperationId | undefined {
    return this.operation;
  }

  toJSON(): ShadeErrorBody {
    return {
      code: this.code,
      retry: this.retry,
      ...(this.operation === undefined ? {} : { operation: this.operation }),
      ...(this.next === undefined ? {} : { next: this.next }),
      ...(this.diagnostics_id === undefined
        ? {}
        : { diagnostics_id: this.diagnostics_id }),
    };
  }
}

export class ShadeTimeoutError extends ShadeError {
  readonly idempotency_key?: string;

  constructor(
    operation?: OperationId,
    options?: ErrorOptions & { idempotency_key?: string },
  ) {
    const idempotencyKey = options?.idempotency_key;
    super(
      {
        code: "CLIENT_TIMEOUT",
        retry:
          operation === undefined && idempotencyKey === undefined
            ? "retry_request"
            : "query_operation",
        ...(operation === undefined ? {} : { operation }),
        next:
          operation !== undefined
            ? "operations.wait(operation_id)"
            : idempotencyKey !== undefined
              ? "operations.waitByKey(idempotency_key)"
              : "reconnect",
      },
      "Shade client deadline exceeded",
      options,
    );
    this.name = "ShadeTimeoutError";
    if (idempotencyKey !== undefined) this.idempotency_key = idempotencyKey;
  }
}

export function asShadeError(error: unknown): ShadeError {
  if (error instanceof ShadeError) return error;
  const message = error instanceof Error ? error.message : String(error);
  return new ShadeError(
    { code: "CLIENT_TRANSPORT", retry: "reconnect", next: "retry" },
    message,
    error instanceof Error ? { cause: error } : undefined,
  );
}
