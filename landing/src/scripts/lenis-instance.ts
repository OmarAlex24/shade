/**
 * Shared Lenis instance registry.
 *
 * Pattern: producer (smooth.ts) calls setLenis() after construction;
 * consumers (Nav.astro, etc.) call onLenisReady() to receive the instance
 * once it is available. This is order-independent — no race condition
 * between smooth.ts and Nav's script evaluation.
 *
 * If Lenis is never initialised (prefers-reduced-motion), registered
 * callbacks are simply never called and consumers fall back to native logic.
 */

import type Lenis from 'lenis';

let _instance: Lenis | null = null;
const _queue: Array<(lenis: Lenis) => void> = [];

/**
 * Called once by smooth.ts immediately after creating the Lenis instance.
 * Drains any callbacks that were registered before init completed.
 */
export function setLenis(lenis: Lenis): void {
  _instance = lenis;
  // Drain the queue — all callbacks registered before init get called now
  let cb: ((lenis: Lenis) => void) | undefined;
  while ((cb = _queue.shift()) !== undefined) {
    cb(lenis);
  }
}

/**
 * Register a callback that fires as soon as the Lenis instance is ready.
 * If Lenis is already initialised, the callback is invoked synchronously.
 * If Lenis is never initialised (reduced-motion), the callback is never called.
 */
export function onLenisReady(callback: (lenis: Lenis) => void): void {
  if (_instance !== null) {
    callback(_instance);
  } else {
    _queue.push(callback);
  }
}
