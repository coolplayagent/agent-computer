import { TextDecoder } from "node:util";
import { Actor, BrowserScope, Viewport } from "./contracts";
import * as validate from "./validation";

/** Private controller/worker protocol, never a public authentication surface. */
export const BRIDGE_LIMITS = Object.freeze({
  input_frame_bytes: 65536, output_frame_bytes: 3 * 1024 * 1024,
  input_bytes: 16 * 1024 * 1024, output_bytes: 64 * 1024 * 1024,
  input_frames: 16384, requests: 1024, handshake_ms: 15000, write_ms: 10000,
});

export interface BridgeBinding {
  readonly scope: BrowserScope;
  readonly actor: Actor;
  readonly viewport: Viewport;
  readonly runtime_ms: number;
}

export class ProtocolError extends Error {
  constructor() { super("invalid_bridge_protocol"); }
}
export function fail(): never { throw new ProtocolError(); }
export function record(value: unknown, keys: readonly string[]): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)
    || Object.getPrototypeOf(value) !== Object.prototype
    || Object.keys(value).length !== keys.length || keys.some((key) => !Object.hasOwn(value, key))) return fail();
  return value as Record<string, unknown>;
}
export function binding(value: unknown): BridgeBinding {
  const v = record(value, ["scope", "actor", "viewport", "runtime_ms"]);
  return Object.freeze({ scope: validate.scope(v.scope), actor: validate.actor(v.actor),
    viewport: validate.viewport(v.viewport), runtime_ms: validate.runtimeLimit(v.runtime_ms) });
}
export function bindingDigest(value: BridgeBinding): string {
  // Separate domain from action intents. Does not authenticate a caller.
  return validate.sha256(Buffer.from(`agent-computer/browser-bridge/v1\0${validate.stableJson(binding(value))}`));
}

/** Incremental bytes, not readline's unbounded string buffer. Strict UTF-8. */
export class FrameReader {
  #buffer = Buffer.alloc(0);
  #bytes = 0;
  #frames = 0;
  push(chunk: Buffer, receive: (value: unknown) => void): void {
    this.#bytes += chunk.length;
    if (this.#bytes > BRIDGE_LIMITS.input_bytes) return fail();
    let start = 0;
    while (start < chunk.length) {
      const end = chunk.indexOf(10, start);
      const part = chunk.subarray(start, end < 0 ? chunk.length : end);
      if (part.length > BRIDGE_LIMITS.input_frame_bytes - this.#buffer.length) return fail();
      this.#buffer = Buffer.concat([this.#buffer, part]);
      if (end < 0) return;
      if (++this.#frames > BRIDGE_LIMITS.input_frames || !this.#buffer.length) return fail();
      let value: unknown;
      try { value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(this.#buffer)); }
      catch { return fail(); }
      this.#buffer = Buffer.alloc(0);
      receive(value);
      start = end + 1;
    }
  }
}

export type Operation =
  | { readonly method: "launch" | "close" }
  | { readonly method: "pages"; readonly actor: Actor }
  | { readonly method: "observe"; readonly actor: Actor; readonly page_id: string }
  | { readonly method: "act"; readonly actor: Actor; readonly action_id: string; readonly action: ReturnType<typeof validate.action> };

export function operation(value: unknown): Operation {
  const method = (value as { method?: unknown } | null)?.method;
  switch (method) {
    case "launch": case "close": record(value, ["method"]); return { method };
    case "pages": {
      const v = record(value, ["method", "actor"]);
      return { method, actor: validate.actor(v.actor) };
    }
    case "observe": {
      const v = record(value, ["method", "actor", "page_id"]);
      return { method, actor: validate.actor(v.actor), page_id: validate.identifier(v.page_id) };
    }
    case "act": {
      const v = record(value, ["method", "actor", "action_id", "action"]);
      return { method, actor: validate.actor(v.actor), action_id: validate.identifier(v.action_id), action: validate.action(v.action) };
    }
    default: return fail();
  }
}
