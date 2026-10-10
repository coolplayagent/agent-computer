import test from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { bridgeBinding, Frame, Peer } from "./bridge-peer";

function processPeer() {
  const process = spawn(global.process.execPath, [join(__dirname, "../src/worker.js"), "--stdio-binding-json", JSON.stringify(bridgeBinding)],
    { stdio: ["pipe", "pipe", "pipe"] });
  let stderr = "";
  process.stderr.on("data", (bytes: Buffer) => { stderr += bytes.toString("utf8"); });
  const exit = once(process, "exit");
  const p = new Peer("/unused", bridgeBinding, undefined, { input: process.stdin, output: process.stdout,
    done: exit.then(([code]) => code === 0 ? "closed" : "protocol_error") });
  return { p, exit, get stderr() { return stderr; }, process };
}
function grant(frame: Frame): object {
  return { ...frame.payload, control_epoch: 1, valid_for_ms: frame.payload.permission === "launch" ? 60000 : 10000 };
}

test("real worker process: callback launch, screenshot bytes, journal order and action replay", { timeout: 60000 }, async () => {
  const f = processPeer(), p = f.p;
  let begun = 0, completed = 0;
  try {
    await p.hello();
    const launch = await p.call({ method: "launch" }); assert.equal(launch.ok, true);
    assert.match(launch.value.browser_version, /^156\./);
    const pages = await p.call({ method: "pages", actor: bridgeBinding.actor }); assert.equal(pages.value.length, 1);
    const observation = await p.call({ method: "observe", actor: bridgeBinding.actor, page_id: pages.value[0].page_id });
    assert.equal(observation.ok, true);
    const png = Buffer.from(observation.value.screenshot_base64, "base64");
    assert.ok(png.length > 100); assert.ok(png.length <= 2 * 1024 * 1024);
    assert.equal(observation.value.screenshot, undefined);
    assert.equal(observation.value.screenshot_sha256, `sha256:${createHash("sha256").update(png).digest("hex")}`);
    const callback = (frame: Frame): unknown => {
      if (frame.method === "authorize") return grant(frame);
      if (frame.method === "begin") {
        begun++; assert.equal(completed, 0); assert.equal(frame.payload.action.kind, "create_page");
        assert.deepEqual(frame.payload.scope, bridgeBinding.scope); return { kind: "dispatch" };
      }
      assert.equal(frame.method, "complete"); completed++; assert.equal(begun, 1);
      assert.equal(frame.payload.outcome, "delivered"); return null;
    };
    const operation = { method: "act", actor: bridgeBinding.actor, action_id: "create_once", action: { kind: "create_page" } };
    const result = await p.call(operation, callback); assert.equal(result.ok, true);
    assert.equal(result.value.recorded, true); assert.equal(result.value.receipt.outcome, "delivered");
    assert.deepEqual((await p.call(operation, callback)).value, result.value);
    assert.equal(begun, 1); assert.equal(completed, 1);
    assert.equal((await p.call({ method: "pages", actor: bridgeBinding.actor })).value.length, 2);
    const closed = await p.call({ method: "close" });
    assert.equal(closed.value.physical_termination, false); assert.equal(closed.value.driver_state, "closed");
    assert.equal(await p.done, "closed"); assert.equal(f.stderr, "");
  } finally { if (f.process.exitCode === null) { p.disconnect(); await f.exit; } }
});

test("real worker process: lost journal completion acknowledgement stays unknown without repeating effect", { timeout: 60000 }, async () => {
  const f = processPeer(), p = f.p;
  try {
    await p.hello(); assert.equal((await p.call({ method: "launch" })).ok, true);
    const operation = { method: "act", actor: bridgeBinding.actor, action_id: "uncertain", action: { kind: "create_page" } };
    p.start(operation);
    let begun = 0, result: Frame;
    for (;;) {
      const frame = await p.next();
      if (frame.kind === "result") { result = frame; break; }
      if (frame.method === "authorize") p.reply(frame, grant(frame));
      else if (frame.method === "begin") { begun++; p.reply(frame, { kind: "dispatch" }); }
      else { assert.equal(frame.method, "complete"); p.reply(frame, null, false); }
    }
    assert.equal(result.ok, true); assert.equal(result.value.recorded, false);
    assert.equal(result.value.receipt.outcome, "unknown"); assert.equal(begun, 1);
    assert.deepEqual((await p.call(operation)).value, result.value);
    const another = await p.call({ ...operation, action_id: "must_not_dispatch" });
    assert.equal(another.ok, false); assert.equal(another.value.code, "control_blocked");
    assert.equal((await p.call({ method: "pages", actor: bridgeBinding.actor })).value.length, 2);
    await p.call({ method: "close" }); assert.equal(await p.done, "closed"); assert.equal(f.stderr, "");
  } finally { if (f.process.exitCode === null) { p.disconnect(); await f.exit; } }
});

test("real worker process: wrong-scope launch grant cannot start Chromium", { timeout: 30000 }, async () => {
  const f = processPeer(), p = f.p;
  try {
    await p.hello();
    const result = await p.call({ method: "launch" }, (frame) => ({ ...grant(frame), scope: { ...bridgeBinding.scope, app_generation: 2 } }));
    assert.equal(result.ok, false); assert.equal(result.value.code, "authority_mismatch");
    assert.equal(await p.done, "protocol_error"); assert.equal(f.stderr, "");
  } finally { if (f.process.exitCode === null) { p.disconnect(); await f.exit; } }
});

test("real worker process: controller disconnect closes the browser process", { timeout: 30000 }, async () => {
  const f = processPeer(), p = f.p;
  try {
    await p.hello(); assert.equal((await p.call({ method: "launch" })).ok, true);
    p.disconnect(); assert.equal(await p.done, "protocol_error"); assert.equal(f.stderr, "");
    // Exit is a local process observation only, never a node-wide fencing proof.
    assert.equal(f.process.exitCode, 1);
  } finally { if (f.process.exitCode === null) { p.disconnect(); await f.exit; } }
});
