import test from "node:test";
import assert from "node:assert/strict";
import { readFile, stat } from "node:fs/promises";
import { join } from "node:path";
import { actor, eventually, fixture } from "./fixture";
import { sha256 } from "../src/validation";

test("real Chromium: screenshots, DOM actions, multi-page isolation and receipt replay", { timeout: 60000 }, async () => {
  const f = await fixture();
  try {
    let observation = await f.navigate();
    const page_id = observation.page_id;
    assert.match(f.driver.browser_version, /^156\./);
    assert.equal(observation.screenshot_sha256, sha256(observation.screenshot));
    assert.deepEqual(observation.image_size, { width: 800, height: 600 });
    assert.equal(observation.coordinate_space, "css_viewport");
    assert.equal(observation.device_scale, 1);
    assert.equal(JSON.stringify(observation.elements).includes("fixture-secret"), false);
    assert.equal((await stat(join(f.root, f.driver.profile_id))).mode & 0o077, 0);
    const button = observation.elements.find((e) => e.text === "Run action")!;
    assert.ok(button);
    assert.throws(() => { (button.bounds as { x: number }).x = 40; }, TypeError);
    const action = { kind: "click", page_id, observation_id: observation.observation_id, element_id: button.element_id };
    const id = f.nextId(), result = await f.driver.act(actor, id, action);
    assert.equal(result.receipt.outcome, "delivered"); assert.equal(result.recorded, true);
    await eventually(() => f.effects === 1);
    assert.deepEqual(await f.driver.act(actor, id, action), result);
    assert.equal(f.effects, 1);
    await assert.rejects(f.driver.act(actor, id, { ...action, kind: "type", text: "x" }), { code: "invalid_request" });
    await assert.rejects(f.driver.act(actor, f.nextId(), action), { code: "stale_observation" });
    observation = await f.driver.observe(actor, page_id);
    const input = observation.elements.find((e) => e.label === "Name")!;
    await f.driver.act(actor, f.nextId(), { kind: "type", page_id, observation_id: observation.observation_id, element_id: input.element_id, text: "typed value" });
    assert.equal((await f.driver.pages(actor))[0]!.title, "typed value");
    observation = await f.driver.observe(actor, page_id);
    await f.driver.act(actor, f.nextId(), { kind: "key", page_id, observation_id: observation.observation_id, key: "Backspace" });
    assert.equal((await f.driver.pages(actor))[0]!.title, "typed valu");
    observation = await f.driver.observe(actor, page_id);
    await f.driver.act(actor, f.nextId(), { kind: "select", page_id, observation_id: observation.observation_id,
      element_id: observation.elements.find((e) => e.label === "Choice")!.element_id, value: "b" });
    assert.equal((await f.driver.pages(actor))[0]!.title, "b");
    const created = await f.driver.act(actor, f.nextId(), { kind: "create_page" });
    assert.equal((await f.driver.pages(actor)).length, 2);
    const second = await f.driver.observe(actor, created.receipt.page_id!);
    assert.equal(second.elements.length, 0);
    await f.driver.act(actor, f.nextId(), { kind: "close_page", page_id: second.page_id, observation_id: second.observation_id });
    assert.equal((await f.driver.pages(actor)).length, 1);
    await f.driver.close();
    assert.equal(f.driver.state, "closed");
    // Local profile exists after close; publication/checkpoint ownership is external.
    assert.ok((await readFile(join(f.root, f.driver.profile_id, "Default", "Preferences"))).length > 0);
  } finally { await f.close(); }
});

test("real Chromium: DOM relocation and changed pixels reject stale targets before intent", { timeout: 60000 }, async () => {
  const f = await fixture();
  try {
    const o = await f.navigate(), button = o.elements.find((e) => e.text === "Run action")!;
    f.mutate("move");
    await eventually(async () => (await f.driver.observe(actor, o.page_id)).elements.find((e) => e.text === "Run action")!.bounds.x !== button.bounds.x);
    const before = f.journal.intents.length;
    await assert.rejects(f.driver.act(actor, f.nextId(), { kind: "click", page_id: o.page_id, observation_id: o.observation_id, element_id: button.element_id }), { code: "stale_observation" });
    await assert.rejects(f.driver.act(actor, f.nextId(), { kind: "click_at", page_id: o.page_id, observation_id: o.observation_id, x: button.bounds.x + 10, y: button.bounds.y + 10 }), { code: "stale_observation" });
    assert.equal(f.journal.intents.length, before); assert.equal(f.effects, 0);
    const fresh = await f.driver.observe(actor, o.page_id), moved = fresh.elements.find((e) => e.text === "Run action")!;
    await f.driver.act(actor, f.nextId(), { kind: "click_at", page_id: o.page_id, observation_id: fresh.observation_id, x: moved.bounds.x + 10, y: moved.bounds.y + 10 });
    await eventually(() => f.effects === 1);
  } finally { await f.close(); }
});

test("real Chromium: navigation, frame recreation and control handoff invalidate observations", { timeout: 60000 }, async () => {
  const f = await fixture();
  try {
    for (const change of ["frame", "navigate", "handoff"]) {
      const o = await f.navigate(), before = f.journal.intents.length;
      if (change === "handoff") f.authority.epoch++;
      else { f.mutate(change); await eventually(async () => (await f.driver.pages(actor))[0]!.document_generation > o.document_generation); }
      await assert.rejects(f.driver.act(actor, f.nextId(), { kind: "scroll", page_id: o.page_id, observation_id: o.observation_id, delta_x: 0, delta_y: 100 }), { code: "stale_observation" });
      assert.equal(f.journal.intents.length, before);
    }
    f.authority.epoch--;
    await assert.rejects(f.driver.pages(actor), { code: "authority_mismatch" });
  } finally { await f.close(); }
});

test("real Chromium: revoked authority denies reads and rechecks after persisted intent", { timeout: 60000 }, async () => {
  const f = await fixture();
  try {
    const o = await f.navigate(), button = o.elements.find((e) => e.text === "Run action")!;
    f.authority.denied = true;
    await assert.rejects(f.driver.pages(actor), { code: "authority_unavailable" });
    await assert.rejects(f.driver.observe(actor, o.page_id), { code: "authority_unavailable" });
    f.authority.denied = false;
    f.journal.beforeBegin = async () => { f.authority.denied = true; };
    const result = await f.driver.act(actor, f.nextId(), { kind: "click", page_id: o.page_id, observation_id: o.observation_id, element_id: button.element_id });
    assert.equal(result.receipt.outcome, "unknown"); assert.equal(result.recorded, true);
    assert.equal(f.effects, 0); assert.equal(f.driver.control_blocked, true);
  } finally { await f.close(); }
});

test("real Chromium: lost completion acknowledgement returns unknown and never repeats effects", { timeout: 60000 }, async () => {
  const f = await fixture();
  try {
    const o = await f.navigate(), id = f.nextId();
    const action = { kind: "click", page_id: o.page_id, observation_id: o.observation_id, element_id: o.elements.find((e) => e.text === "Run action")!.element_id };
    f.journal.loseComplete = true;
    const result = await f.driver.act(actor, id, action);
    assert.equal(result.recorded, false); assert.equal(result.receipt.outcome, "unknown");
    assert.equal(f.journal.receipts.at(-1)!.outcome, "delivered");
    await eventually(() => f.effects === 1);
    assert.deepEqual(await f.driver.act(actor, id, action), result);
    await assert.rejects(f.driver.act(actor, f.nextId(), { kind: "create_page" }), { code: "control_blocked" });
    assert.ok((await f.driver.observe(actor, o.page_id)).elements.some((e) => e.text === "clicked"));
    assert.equal(f.effects, 1);
  } finally { await f.close(); }
});

test("real Chromium: one in-flight operation and bounded observation lifetime", { timeout: 60000 }, async () => {
  const f = await fixture();
  try {
    const o = await f.navigate();
    let release!: () => void;
    const wait = new Promise<void>((resolve) => { release = resolve; });
    f.authority.before = async () => wait;
    const pending = f.driver.pages(actor);
    await assert.rejects(f.driver.pages(actor), { code: "busy" });
    release(); await pending; f.authority.before = undefined;
    f.authority.validFor = 500;
    const expiring = await f.driver.observe(actor, o.page_id);
    await new Promise((resolve) => setTimeout(resolve, expiring.valid_for_ms + 20));
    await assert.rejects(f.driver.act(actor, f.nextId(), { kind: "scroll", page_id: o.page_id, observation_id: expiring.observation_id, delta_x: 0, delta_y: 20 }), { code: "stale_observation" });
  } finally { await f.close(); }
});

test("real Chromium: page quota rejects before intent and runtime closes the context", { timeout: 60000 }, async () => {
  const f = await fixture();
  try {
    for (let i = 1; i < 8; i++) await f.driver.act(actor, f.nextId(), { kind: "create_page" });
    const before = f.journal.intents.length;
    await assert.rejects(f.driver.act(actor, f.nextId(), { kind: "create_page" }), { code: "page_limit" });
    assert.equal(f.journal.intents.length, before);
  } finally { await f.close(); }
  const limited = await fixture(2000);
  try { await eventually(() => limited.driver.state === "closed"); await assert.rejects(limited.driver.pages(actor), { code: "closed" }); }
  finally { await limited.close(); }
});

test("real Chromium: journal unavailable or late admission never dispatches", { timeout: 60000 }, async () => {
  const f = await fixture();
  try {
    const o = await f.navigate();
    f.journal.failBegin = true;
    await assert.rejects(f.driver.act(actor, f.nextId(), { kind: "click", page_id: o.page_id,
      observation_id: o.observation_id, element_id: o.elements.find((e) => e.text === "Run action")!.element_id }), { code: "journal_unavailable" });
    assert.equal(f.effects, 0); assert.equal(f.driver.control_blocked, true);
  } finally { await f.close(); }
  const slow = await fixture();
  try {
    const o = await slow.navigate();
    slow.journal.beforeBegin = async () => new Promise((resolve) => setTimeout(resolve, 10200));
    await assert.rejects(slow.driver.act(actor, slow.nextId(), { kind: "click", page_id: o.page_id,
      observation_id: o.observation_id, element_id: o.elements.find((e) => e.text === "Run action")!.element_id }), { code: "browser_unavailable" });
    await new Promise((resolve) => setTimeout(resolve, 300));
    assert.equal(slow.effects, 0); assert.equal(slow.driver.control_blocked, true);
    assert.equal(slow.journal.receipts.length, 1); // navigation only
  } finally { await slow.close(); }
});

test("real Chromium: bounded traversal and page-created tabs", { timeout: 60000 }, async () => {
  const f = await fixture();
  try {
    f.setMode("large");
    const o = await f.navigate();
    assert.equal(o.dom_truncated, true);
    assert.ok(o.elements.length <= 100);
    assert.ok(Buffer.byteLength(JSON.stringify(o.elements)) < 40000);
    await f.driver.act(actor, f.nextId(), { kind: "click", page_id: o.page_id,
      observation_id: o.observation_id, element_id: o.elements.find((e) => e.text === "New page")!.element_id });
    await eventually(async () => (await f.driver.pages(actor)).length === 2);
    const pages = await f.driver.pages(actor);
    assert.notEqual(pages[0]!.page_id, pages[1]!.page_id);
  } finally { await f.close(); }
});

test("real Chromium: malformed journal admission fails closed and persisted Unknown is never replayed", { timeout: 60000 }, async () => {
  for (const malformed of [true, false]) {
    const f = await fixture();
    try {
      const o = await f.navigate();
      if (malformed) f.journal.begin = async () => null as never;
      else f.journal.beforeBegin = async () => {
        const intent = f.journal.intents.at(-1)!;
        f.journal.replay = { action_id: intent.action_id, input_digest: intent.input_digest,
          app_instance_id: intent.scope.app_instance_id, app_generation: intent.scope.app_generation,
          control_epoch: intent.control_epoch, outcome: "unknown" };
      };
      const operation = f.driver.act(actor, f.nextId(), { kind: "click", page_id: o.page_id,
        observation_id: o.observation_id, element_id: o.elements.find((e) => e.text === "Run action")!.element_id });
      if (malformed) await assert.rejects(operation, { code: "journal_unavailable" });
      else assert.equal((await operation).receipt.outcome, "unknown");
      assert.equal(f.effects, 0); assert.equal(f.driver.control_blocked, true);
      await assert.rejects(f.driver.act(actor, f.nextId(), { kind: "create_page" }), { code: "control_blocked" });
    } finally { await f.close(); }
  }
});

test("real Chromium: collecting target proof cannot extend observation validity", { timeout: 60000 }, async () => {
  const f = await fixture();
  try {
    const first = await f.navigate();
    f.authority.validFor = 2000;
    const o = await f.driver.observe(actor, first.page_id);
    f.authority.validFor = 30000;
    f.mutate("slow-geometry"); await eventually(() => f.slowReady);
    const before = f.journal.intents.length;
    await assert.rejects(f.driver.act(actor, f.nextId(), { kind: "click", page_id: o.page_id,
      observation_id: o.observation_id, element_id: o.elements.find((e) => e.text === "Run action")!.element_id }), { code: "stale_observation" });
    assert.equal(f.journal.intents.length, before); assert.equal(f.effects, 0);
    assert.equal(f.driver.control_blocked, false);
  } finally { await f.close(); }
});

test("real Chromium: serial control drain invalidates old observations before handoff", { timeout: 60000 }, async () => {
  const f = await fixture();
  try {
    const old = await f.navigate();
    const barrier = await f.driver.drain(f.authority.epoch);
    assert.deepEqual(barrier, { control_epoch: f.authority.epoch, drained: true, requires_observation: true });
    await assert.rejects(f.driver.act(actor, f.nextId(), { kind: "click_at", page_id: old.page_id,
      observation_id: old.observation_id, x: 10, y: 10 }), { code: "stale_observation" });
    f.authority.epoch++;
    const fresh = await f.driver.observe(actor, old.page_id);
    assert.equal(fresh.control_epoch, f.authority.epoch);
    await assert.rejects(f.driver.drain(f.authority.epoch - 1), { code: "invalid_request" });
    f.journal.loseComplete = true;
    await f.driver.act(actor, f.nextId(), { kind: "create_page" });
    await assert.rejects(f.driver.drain(f.authority.epoch), { code: "control_blocked" });
  } finally { await f.close(); }
});
