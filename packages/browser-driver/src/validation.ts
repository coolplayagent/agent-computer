import { createHash } from "node:crypto";
import { Action, Actor, BrowserScope, DriverError, LIMITS, Viewport } from "./contracts";

export function invalid(): never { throw new DriverError("invalid_request"); }

function record(value: unknown, keys: readonly string[]): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) return invalid();
  if (![Object.prototype, null].includes(Object.getPrototypeOf(value))) return invalid();
  if (Object.values(Object.getOwnPropertyDescriptors(value)).some((field) => field.get || field.set)) return invalid();
  if (Object.keys(value).some((key) => !keys.includes(key))) return invalid();
  return value as Record<string, unknown>;
}

export function identifier(value: unknown): string {
  if (typeof value !== "string" || !/^[a-zA-Z0-9_-]{1,128}$/.test(value)) return invalid();
  return value;
}

function integer(value: unknown, min: number, max: number): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < min || value > max) return invalid();
  return value;
}

function numeric(value: unknown, min: number, max: number): number {
  if (typeof value !== "number" || !Number.isFinite(value) || value < min || value > max) return invalid();
  return value;
}

function text(value: unknown, maximum: number): string {
  if (typeof value !== "string" || Buffer.byteLength(value, "utf8") > maximum || value.includes("\0")) return invalid();
  return value;
}

export function scope(value: unknown): BrowserScope {
  const v = record(value, ["organization_id", "computer_id", "computer_generation", "app_instance_id", "app_generation", "profile_owner_id"]);
  return Object.freeze({
    organization_id: identifier(v.organization_id), computer_id: identifier(v.computer_id),
    computer_generation: integer(v.computer_generation, 1, Number.MAX_SAFE_INTEGER),
    app_instance_id: identifier(v.app_instance_id), app_generation: integer(v.app_generation, 1, Number.MAX_SAFE_INTEGER),
    profile_owner_id: identifier(v.profile_owner_id),
  });
}

export function actor(value: unknown): Actor {
  const v = record(value, ["principal_id", "connection_session_id"]);
  return Object.freeze({ principal_id: identifier(v.principal_id), connection_session_id: identifier(v.connection_session_id) });
}

export function viewport(value: unknown): Viewport {
  const v = record(value, ["width", "height"]);
  const width = integer(v.width, 320, 1920), height = integer(v.height, 240, 1080);
  return Object.freeze({ width, height });
}

export function runtimeLimit(value: unknown): number { return integer(value, 1_000, LIMITS.runtime_ms); }

export function action(value: unknown): Action {
  const base = record(value, ["kind", "page_id", "observation_id", "url", "element_id", "x", "y", "text", "value", "key", "delta_x", "delta_y"]);
  const shape = (extra: string[]) => record(value, ["kind", "page_id", "observation_id", ...extra]);
  if (base.kind === "create_page") {
    record(value, ["kind"]);
    return Object.freeze({ kind: "create_page" });
  }
  const page_id = identifier(base.page_id), observation_id = identifier(base.observation_id);
  switch (base.kind) {
    case "close_page":
      shape([]); return Object.freeze({ kind: "close_page", page_id, observation_id });
    case "navigate": {
      const v = shape(["url"]);
      const input = text(v.url, 4096);
      let url: URL;
      try { url = new URL(input); } catch { return invalid(); }
      if (!["http:", "https:"].includes(url.protocol) || url.username || url.password) return invalid();
      return Object.freeze({ kind: "navigate", page_id, observation_id, url: url.href });
    }
    case "click": {
      const v = shape(["element_id"]);
      return Object.freeze({ kind: "click", page_id, observation_id, element_id: identifier(v.element_id) });
    }
    case "click_at": {
      const v = shape(["x", "y"]);
      return Object.freeze({ kind: "click_at", page_id, observation_id, x: numeric(v.x, 0, 1920), y: numeric(v.y, 0, 1080) });
    }
    case "type": {
      const v = shape(["element_id", "text"]);
      return Object.freeze({ kind: "type", page_id, observation_id, element_id: identifier(v.element_id), text: text(v.text, LIMITS.text_bytes) });
    }
    case "select": {
      const v = shape(["element_id", "value"]);
      return Object.freeze({ kind: "select", page_id, observation_id, element_id: identifier(v.element_id), value: text(v.value, 1024) });
    }
    case "key": {
      const v = shape(["key"]), key = text(v.key, 32);
      if (!["Enter", "Tab", "Escape", "Backspace", "Delete", "ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End", "PageUp", "PageDown", "Space"].includes(key)) return invalid();
      return Object.freeze({ kind: "key", page_id, observation_id, key });
    }
    case "scroll": {
      const v = shape(["delta_x", "delta_y"]);
      return Object.freeze({ kind: "scroll", page_id, observation_id, delta_x: numeric(v.delta_x, -2000, 2000), delta_y: numeric(v.delta_y, -2000, 2000) });
    }
    default: return invalid();
  }
}

export function stableJson(value: unknown): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(stableJson).join(",")}]`;
  return `{${Object.entries(value).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0)
    .map(([key, item]) => `${JSON.stringify(key)}:${stableJson(item)}`).join(",")}}`;
}

export function digest(value: unknown): string {
  return `sha256:${createHash("sha256").update("agent-computer/browser-action/v1\0").update(stableJson(value)).digest("hex")}`;
}

export function sha256(value: Uint8Array): string { return `sha256:${createHash("sha256").update(value).digest("hex")}`; }
