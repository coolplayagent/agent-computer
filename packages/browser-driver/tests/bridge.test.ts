import test from "node:test";
import assert from "node:assert/strict";
import { DriverError } from "../src/contracts";
import { BRIDGE_LIMITS, FrameReader, binding, bindingDigest } from "../src/bridge-protocol";
import { DriverConfiguration } from "../src/driver";
import { bridgeBinding, Peer } from "./bridge-peer";

function fake() {
  let launched = 0, closed = 0, effects = 0;
  const launch = async (c: DriverConfiguration) => {
    launched++;
    const signal = new AbortController().signal;
    await c.authority.authorize({ scope: c.scope, actor: c.actor, permission: "launch" }, signal);
    return { scope: c.scope, browser_version: "fixture", state: "open", control_blocked: false,
      pages: async () => [{ page_id: "page", document_generation: 1, title: "fixture", url: "about:blank" }],
      observe: async () => { throw new DriverError("page_unavailable"); },
      act: async () => {
        const intent = { scope: c.scope, actor: c.actor, control_epoch: 1, action_id: "action",
          input_digest: "sha256:fixture", action: { kind: "create_page" as const } };
        await c.journal.begin(intent, signal); effects++;
        const receipt = { action_id: intent.action_id, input_digest: intent.input_digest,
          app_instance_id: c.scope.app_instance_id, app_generation: c.scope.app_generation,
          control_epoch: 1, outcome: "delivered" as const };
        await c.journal.complete(receipt, signal); return { receipt, recorded: true };
      },
      close: async () => { closed++; },
    };
  };
  return { launch, get launched() { return launched; }, get closed() { return closed; }, get effects() { return effects; } };
}

test("bridge codec bounds unterminated input and rejects malformed UTF-8/JSON", () => {
  const reader = new FrameReader(), values: unknown[] = [];
  const bytes = Buffer.from('{"text":"中文"}\n{}\n');
  for (const byte of bytes) reader.push(Buffer.from([byte]), (value) => values.push(value));
  assert.deepEqual(values, [{ text: "中文" }, {}]);
  for (const input of [Buffer.alloc(BRIDGE_LIMITS.input_frame_bytes + 1, 32),
    Buffer.from([34, 0xff, 34, 10]), Buffer.from("null\0\n"), Buffer.from("\n")]) {
    assert.throws(() => new FrameReader().push(input, () => {}), { message: "invalid_bridge_protocol" });
  }
});

test("bridge binding is strict and binds all runtime identities and limits", () => {
  for (const value of [{ ...bridgeBinding, profile_root: "/escape" },
    { ...bridgeBinding, runtime_ms: 300001 }, { ...bridgeBinding, actor: { ...bridgeBinding.actor, token: "secret" } }]) {
    assert.throws(() => binding(value));
  }
  for (const value of [{ ...bridgeBinding, runtime_ms: 50000 },
    { ...bridgeBinding, scope: { ...bridgeBinding.scope, app_generation: 2 } },
    { ...bridgeBinding, actor: { ...bridgeBinding.actor, principal_id: "other" } }]) {
    assert.notEqual(bindingDigest(bridgeBinding), bindingDigest(value));
  }
});

test("bridge refuses an unbound hello before attempting launch", async () => {
  for (const hello of [{ version: 1, kind: "hello", binding_digest: "wrong" },
    { version: 1, kind: "hello", binding_digest: bindingDigest(bridgeBinding), extra: true }]) {
    const f = fake(), p = new Peer("/unused", bridgeBinding, f.launch);
    p.send(hello); assert.equal(await p.done, "protocol_error"); assert.equal(f.launched, 0);
  }
});

test("bridge requires a callback before launch result and never claims physical termination", async () => {
  const f = fake(), p = new Peer("/unused", bridgeBinding, f.launch);
  await p.hello(); p.start({ method: "launch" });
  const request = await p.next(); assert.equal(request.kind, "callback"); assert.equal(request.method, "authorize");
  assert.deepEqual(request.payload, { scope: bridgeBinding.scope, actor: bridgeBinding.actor, permission: "launch" });
  assert.equal(p.frames.some((frame) => frame.kind === "result"), false);
  p.reply(request, { ...request.payload, control_epoch: 1, valid_for_ms: 10000 });
  assert.equal((await p.next()).ok, true);
  // Deliberately send immediately after reading the result, before a delayed
  // Writable callback could finish. This must not look like request pipelining.
  assert.equal((await p.call({ method: "pages", actor: bridgeBinding.actor })).ok, true);
  assert.deepEqual((await p.call({ method: "close" })).value, { driver_state: "open", physical_termination: false });
  assert.equal(await p.done, "closed"); assert.ok(f.closed >= 1);
});

test("bridge rejects replayed or cross-session callback identities", async () => {
  for (const changed of [{ session_id: "other" }, { request_id: 2 }, { callback_id: 2 },
    { method: "begin" }, { version: 2 }, { extra: true }, { ok: "yes" }, { ok: false, value: "private error" }]) {
    const f = fake(), p = new Peer("/unused", bridgeBinding, f.launch);
    await p.hello(); p.start({ method: "launch" }); const callback = await p.next();
    p.reply(callback, {}, true, changed);
    assert.equal(await p.done, "protocol_error"); assert.equal(f.effects, 0);
    assert.equal(p.frames.some((frame) => frame.kind === "result"), false);
  }
});

test("bridge disconnect cancels a pending authority callback", async () => {
  const f = fake(), p = new Peer("/unused", bridgeBinding, f.launch);
  await p.hello(); p.start({ method: "launch" }); await p.next(); p.disconnect();
  assert.equal(await p.done, "transport_error"); assert.equal(f.effects, 0);
});

test("bridge rejects pipelining while awaiting authority and repeated operation IDs", async () => {
  for (const active of [true, false]) {
    const f = fake(), p = new Peer("/unused", bridgeBinding, f.launch);
    await p.hello();
    if (active) { p.start({ method: "launch" }); await p.next(); }
    else { await p.call({ method: "launch" }); p.request--; }
    p.start({ method: "pages", actor: bridgeBinding.actor });
    assert.equal(await p.done, "protocol_error");
  }
});

test("bridge forwards journal intent before effect, disconnect after completion never replays", async () => {
  const f = fake(), p = new Peer("/unused", bridgeBinding, f.launch);
  await p.hello(); await p.call({ method: "launch" });
  p.start({ method: "act", actor: bridgeBinding.actor, action_id: "action", action: { kind: "create_page" } });
  const intent = await p.next(); assert.equal(intent.method, "begin"); assert.equal(f.effects, 0);
  p.reply(intent, { kind: "dispatch" });
  const receipt = await p.next(); assert.equal(receipt.method, "complete"); assert.equal(f.effects, 1);
  p.disconnect(); assert.equal(await p.done, "transport_error");
  assert.equal(f.effects, 1); assert.ok(f.closed >= 1);
});

test("bridge sanitizes driver failures and a failed launch is terminal", async () => {
  const p = new Peer("/unused", bridgeBinding, async () => { throw new Error("secret filesystem/token"); });
  await p.hello(); const result = await p.call({ method: "launch" });
  assert.deepEqual(result.value, { code: "bridge_failure" });
  assert.equal(await p.done, "protocol_error"); assert.equal(JSON.stringify(p.frames).includes("secret"), false);
});

test("bridge idle runtime budget expires without a callback or browser launch", async () => {
  const config = { ...bridgeBinding, runtime_ms: 1000 };
  const f = fake(), p = new Peer("/unused", config, f.launch);
  await p.hello(config); assert.equal(await p.done, "deadline"); assert.equal(f.launched, 0);
});

test("bridge rejects unknown operations, extra fields and path injection", async () => {
  for (const operation of [{ method: "exec", command: "id" },
    { method: "launch", profile_root: "/etc" }, { method: "launch", argv: ["--no-sandbox"] }]) {
    const f = fake(), p = new Peer("/unused", bridgeBinding, f.launch);
    await p.hello(); p.start(operation); assert.equal(await p.done, "protocol_error"); assert.equal(f.launched, 0);
  }
});

test("bridge denies a duplicate callback after the original launch result", async () => {
  const f = fake(), p = new Peer("/unused", bridgeBinding, f.launch);
  await p.hello(); await p.call({ method: "launch" });
  const callback = p.frames.find((frame) => frame.kind === "callback")!;
  p.reply(callback, {}); assert.equal(await p.done, "protocol_error"); assert.equal(f.effects, 0);
});

test("bridge abort signal ends a pending callback and truncated hello never launches", async () => {
  const f = fake();
  const p = new Peer("/unused", bridgeBinding, async (config) => {
    const controller = new AbortController();
    const pending = config.authority.authorize({ scope: config.scope, actor: config.actor, permission: "launch" }, controller.signal);
    controller.abort(); await pending; return f.launch(config);
  });
  await p.hello(); p.start({ method: "launch" }); assert.equal(await p.done, "deadline");
  const incomplete = new Peer("/unused", bridgeBinding, f.launch);
  incomplete.input.write('{"version":1'); incomplete.disconnect();
  assert.equal(await incomplete.done, "transport_error"); assert.equal(f.launched, 0);
});

test("bridge bounds cumulative input as well as a single frame", () => {
  const reader = new FrameReader();
  const line = Buffer.from(`${JSON.stringify("x".repeat(65000))}\n`);
  let accepted = 0;
  assert.throws(() => {
    for (let i = 0; i < 300; i++) reader.push(line, () => { accepted++; });
  }, { message: "invalid_bridge_protocol" });
  assert.ok(accepted > 200); assert.ok(accepted < 300);
});
