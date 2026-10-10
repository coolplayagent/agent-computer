import assert from "node:assert/strict";
import { PassThrough, Readable, Writable } from "node:stream";
import { once } from "node:events";
import { BridgeExit, serveBridge } from "../src/bridge";
import { bindingDigest, BridgeBinding } from "../src/bridge-protocol";

export const bridgeBinding: BridgeBinding = {
  scope: { organization_id: "org", computer_id: "computer", computer_generation: 1,
    app_instance_id: "app", app_generation: 1, profile_owner_id: "owner" },
  actor: { principal_id: "owner", connection_session_id: "connection" },
  viewport: { width: 800, height: 600 }, runtime_ms: 60000,
};
// Test-only peer. Production controllers must authenticate their own caller,
// compare every callback to that operation and persist intents before replying.
export type Frame = Record<string, any>;
export class Peer {
  readonly input: Writable;
  readonly output: Readable;
  readonly frames: Frame[] = [];
  readonly done: ReturnType<typeof serveBridge>;
  session = "";
  request = 0;
  #buffer = "";
  #queued: Frame[] = [];
  constructor(root: string, config = bridgeBinding, launch?: Parameters<typeof serveBridge>[3],
    transport?: { input: Writable; output: Readable; done: Promise<BridgeExit> }) {
    this.input = transport?.input ?? new PassThrough();
    this.output = transport?.output ?? new PassThrough();
    this.output.on("data", (bytes: Buffer) => {
      this.#buffer += bytes.toString("utf8");
      for (let end; (end = this.#buffer.indexOf("\n")) >= 0;) {
        const frame = JSON.parse(this.#buffer.slice(0, end)) as Frame;
        this.#buffer = this.#buffer.slice(end + 1);
        this.frames.push(frame); this.#queued.push(frame);
        this.output.emit("frame");
      }
    });
    this.done = transport?.done ?? serveBridge({ ...config, profile_root: root }, this.input as PassThrough, this.output as PassThrough, launch);
  }
  send(value: unknown): void { this.input.write(Buffer.from(`${JSON.stringify(value)}\n`)); }
  async next(): Promise<Frame> {
    if (!this.#queued.length) await once(this.output, "frame", { signal: AbortSignal.timeout(15000) });
    const frame = this.#queued.shift(); assert.ok(frame); return frame;
  }
  async hello(config = bridgeBinding): Promise<void> {
    this.send({ version: 1, kind: "hello", binding_digest: bindingDigest(config) });
    const challenge = await this.next(); assert.equal(challenge.kind, "challenge");
    assert.equal(challenge.binding_digest, bindingDigest(config));
    this.session = challenge.session_id;
  }
  start(operation: object): void {
    this.send({ version: 1, kind: "request", session_id: this.session, request_id: ++this.request, operation });
  }
  reply(frame: Frame, value: unknown, ok = true, overrides = {}): void {
    this.send({ version: 1, kind: "reply", session_id: this.session, request_id: frame.request_id,
      callback_id: frame.callback_id, method: frame.method, ok, value, ...overrides });
  }
  async call(operation: object, callback: (frame: Frame) => unknown = (frame) => {
    assert.equal(frame.method, "authorize");
    return { ...frame.payload, control_epoch: 1, valid_for_ms: frame.payload.permission === "launch" ? 60000 : 10000 };
  }): Promise<Frame> {
    this.start(operation);
    for (;;) {
      const frame = await this.next();
      assert.equal(frame.request_id, this.request);
      if (frame.kind === "result") return frame;
      assert.equal(frame.kind, "callback"); this.reply(frame, await callback(frame));
    }
  }
  disconnect(): void { this.input.end(); }
}
