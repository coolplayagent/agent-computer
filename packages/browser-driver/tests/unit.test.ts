import test from "node:test";
import assert from "node:assert/strict";
import { performance } from "node:perf_hooks";
import { authorize } from "../src/authority";
import { Actor, Authority, BrowserScope, DriverError } from "../src/contracts";
import { action, digest, scope } from "../src/validation";
import { parsePayload } from "../src/observation";

const identity: BrowserScope = { organization_id: "org", computer_id: "computer", computer_generation: 1,
  app_instance_id: "app", app_generation: 1, profile_owner_id: "owner" };
const caller: Actor = { principal_id: "owner", connection_session_id: "connection" };
const valid = { page_id: "page", observation_id: "observation" };
const invalid = (run: () => unknown) => assert.throws(run, (error) => error instanceof DriverError && error.code === "invalid_request");

test("actions reject extra commands, unbounded text, unsafe URLs and numeric input", () => {
  for (const input of [
    { kind: "evaluate", script: "document.cookie" }, { kind: "create_page", url: "https://example.test" },
    { kind: "click", ...valid, element_id: "element", selector: "button" },
    ...["file:///etc/passwd", "javascript:alert(1)", "data:text/html,x", "https://user:secret@example.test"].map((url) => ({ kind: "navigate", ...valid, url })),
    { kind: "type", ...valid, element_id: "e", text: "汉".repeat(6000) },
    { kind: "click_at", ...valid, x: NaN, y: 0 },
    { kind: "key", ...valid, key: "Control+L" },
    Object.assign(Object.create({ unexpected: true }), { kind: "create_page" }),
  ]) invalid(() => action(input));
  let accessed = false;
  invalid(() => action({ get kind() { accessed = true; return "create_page"; } }));
  assert.equal(accessed, false);
  assert.equal(action({ kind: "navigate", ...valid, url: "https://example.test" }).kind, "navigate");
});

test("action digests bind actor, epoch and full input independently of key order", () => {
  const a = { scope: identity, actor: caller, epoch: 1, action: { kind: "type", text: "a" } };
  assert.equal(digest(a), digest({ action: a.action, epoch: 1, actor: caller, scope: identity }));
  assert.notEqual(digest(a), digest({ ...a, epoch: 2 }));
  assert.notEqual(digest(a), digest({ ...a, actor: { ...caller, principal_id: "other" } }));
  assert.notEqual(digest(a), digest({ ...a, action: { ...a.action, text: "b" } }));
  invalid(() => scope({ ...identity, computer_generation: 0 }));
});

test("authority rejects mismatched scope, permission, actor, epoch, budget and abort", async () => {
  for (const changed of [{ scope: { ...identity, app_generation: 2 } }, { actor: { ...caller, principal_id: "other" } },
    { permission: "observe" }, { control_epoch: 0 }, { valid_for_ms: 30001 }]) {
    const authority = { authorize: async (request: object) => ({ ...request, control_epoch: 1, valid_for_ms: 1000, ...changed }) } as Authority;
    await assert.rejects(authorize(authority, identity, caller, "control", performance.now() + 1000, new AbortController().signal), { code: "authority_mismatch" });
  }
  const controller = new AbortController(); controller.abort();
  const authority: Authority = { authorize: async (request) => ({ ...request, control_epoch: 1, valid_for_ms: 1000 }) };
  await assert.rejects(authorize(authority, identity, caller, "control", performance.now() + 1000, controller.signal), { code: "authority_mismatch" });
});

test("waiting for authority consumes the grant and failure never grants access", async () => {
  const authority: Authority = { authorize: async (request) => {
    await new Promise((resolve) => setTimeout(resolve, 30));
    return { ...request, control_epoch: 1, valid_for_ms: 10 };
  } };
  await assert.rejects(authorize(authority, identity, caller, "control", performance.now() + 1000, new AbortController().signal), { code: "authority_unavailable" });
  await assert.rejects(authorize({ authorize: async () => { throw new Error("private URL"); } }, identity, caller,
    "observe", performance.now() + 1000, new AbortController().signal), { message: "authority_unavailable" });
});

test("renderer payload is independently bounded, normalized and immutable", () => {
  const entry = { tag: "button", role: "", label: "label", text: "text", bounds: { x: 2, y: 3, width: 20, height: 30 } };
  const value = { entries: [entry], truncated: false, metrics: { width: 800, height: 600, device_scale: 1, scroll_x: 0, scroll_y: 0 } };
  const result = parsePayload(value);
  entry.bounds.x = 50;
  assert.equal(result.entries[0]!.bounds.x, 2);
  assert.throws(() => { result.entries[0]!.bounds.x = 20; }, TypeError);
  for (const changed of [{ entries: Array(101).fill(entry) }, { metrics: { ...value.metrics, width: Infinity } },
    { entries: [{ ...entry, text: "x".repeat(513) }] }, { truncated: "yes" },
    { entries: Array(100).fill({ ...entry, label: "汉".repeat(512), text: "汉".repeat(512) }) }]) {
    assert.throws(() => parsePayload({ ...value, ...changed }), { code: "observation_too_large" });
  }
});
