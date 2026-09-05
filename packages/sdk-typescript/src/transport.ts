import { createConnection, type Socket } from "node:net";

import { ShadeError, ShadeTimeoutError, asShadeError } from "./errors.ts";
import type { WireRequest } from "./protocol.ts";

const DEFAULT_MAX_FRAME_BYTES = 4 * 1024 * 1024;

export interface TransportOptions {
  socket: string;
  max_frame_bytes?: number;
}

export class NdjsonUnixTransport {
  readonly socket: string;
  readonly max_frame_bytes: number;

  constructor(options: TransportOptions) {
    this.socket = options.socket;
    this.max_frame_bytes = options.max_frame_bytes ?? DEFAULT_MAX_FRAME_BYTES;
  }

  async request(request: WireRequest, timeoutMs: number): Promise<unknown> {
    const socket = createConnection({ path: this.socket });
    let settled = false;

    return await new Promise((resolve, reject) => {
      let frame = "";
      const timer = setTimeout(() => {
        finish(() => reject(new ShadeTimeoutError()));
      }, Math.max(1, timeoutMs));

      const cleanup = () => {
        clearTimeout(timer);
        socket.removeAllListeners();
        if (!socket.destroyed) socket.destroy();
      };

      const finish = (callback: () => void) => {
        if (settled) return;
        settled = true;
        cleanup();
        callback();
      };

      socket.setEncoding("utf8");
      socket.once("connect", () => {
        socket.write(`${JSON.stringify(request)}\n`);
      });
      socket.on("data", (chunk: string) => {
        frame += chunk;
        if (Buffer.byteLength(frame, "utf8") > this.max_frame_bytes) {
          finish(() =>
            reject(
              new ShadeError({
                code: "CLIENT_FRAME_TOO_LARGE",
                retry: "never",
              }),
            ),
          );
          return;
        }

        const newline = frame.indexOf("\n");
        if (newline < 0) return;
        const line = frame.slice(0, newline);
        try {
          const parsed: unknown = JSON.parse(line);
          finish(() => resolve(parsed));
        } catch (error) {
          finish(() =>
            reject(
              new ShadeError(
                { code: "CLIENT_INVALID_NDJSON", retry: "never" },
                "Daemon returned invalid NDJSON",
                { cause: error },
              ),
            ),
          );
        }
      });
      socket.once("error", (error) => {
        finish(() => reject(asShadeError(error)));
      });
      socket.once("end", () => {
        finish(() =>
          reject(
            new ShadeError({
              code: "CLIENT_EOF",
              retry: "reconnect",
              next: "retry",
            }),
          ),
        );
      });
    });
  }

  async *stream(
    request: WireRequest,
    signal?: AbortSignal,
  ): AsyncGenerator<unknown, void, void> {
    if (signal?.aborted) return;
    const socket = await this.connect(signal);
    let frame = "";

    const abort = () => socket.destroy();
    signal?.addEventListener("abort", abort, { once: true });
    socket.setEncoding("utf8");
    socket.write(`${JSON.stringify(request)}\n`);

    try {
      for await (const chunk of socket) {
        if (signal?.aborted) return;
        frame += String(chunk);
        if (Buffer.byteLength(frame, "utf8") > this.max_frame_bytes) {
          throw new ShadeError({
            code: "CLIENT_FRAME_TOO_LARGE",
            retry: "never",
          });
        }

        for (;;) {
          const newline = frame.indexOf("\n");
          if (newline < 0) break;
          const line = frame.slice(0, newline);
          frame = frame.slice(newline + 1);
          if (line.length === 0) continue;
          try {
            yield JSON.parse(line) as unknown;
          } catch (error) {
            throw new ShadeError(
              { code: "CLIENT_INVALID_NDJSON", retry: "never" },
              "Daemon returned invalid NDJSON",
              { cause: error },
            );
          }
        }
      }
    } finally {
      signal?.removeEventListener("abort", abort);
      if (!socket.destroyed) socket.destroy();
    }
  }

  private async connect(signal?: AbortSignal): Promise<Socket> {
    const socket = createConnection({ path: this.socket });
    const abort = () => socket.destroy();
    signal?.addEventListener("abort", abort, { once: true });
    try {
      await new Promise<void>((resolve, reject) => {
        socket.once("connect", resolve);
        socket.once("error", reject);
      });
      return socket;
    } catch (error) {
      if (!socket.destroyed) socket.destroy();
      throw asShadeError(error);
    } finally {
      signal?.removeEventListener("abort", abort);
    }
  }
}
