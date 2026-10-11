import { randomUUID } from "node:crypto";
import { Readable, Writable } from "node:stream";
import { ActionIntent, ActionReceipt, AuthorizationRequest, DriverError, Grant, LIMITS } from "./contracts";
import { BrowserDriver, DriverConfiguration } from "./driver";
import { binding, bindingDigest, BridgeBinding, BRIDGE_LIMITS, fail, FrameReader, operation, Operation, record } from "./bridge-protocol";

export interface BridgeConfiguration extends BridgeBinding {
  /** Operator-owned private directory. Never read from the wire. */
  readonly profile_root: string;
}
type Driver = Pick<BrowserDriver, "scope" | "browser_version" | "state" | "control_blocked" | "pages" | "observe" | "act" | "drain" | "close">;
type Launch = (configuration: DriverConfiguration) => Promise<Driver>;
export type BridgeExit = "closed" | "protocol_error" | "transport_error" | "deadline";
type CallbackMethod = "authorize" | "begin" | "complete";
interface Pending {
  readonly id: number;
  readonly method: CallbackMethod;
  readonly resolve: (value: unknown) => void;
  readonly reject: () => void;
}

/** One authenticated-controller-owned duplex pipe, one finite Browser process.
 * Wire identities are correlation only. The external controller must compare
 * callbacks with its admitted operation and check current authority itself.
 * No serialized response can prove node termination or release a profile slot.
 */
export async function serveBridge(
  configuration: BridgeConfiguration, input: Readable, output: Writable,
  launch: Launch = BrowserDriver.launch,
): Promise<BridgeExit> {
  const config = binding(configurationBinding(configuration));
  const session = new Session(config, configuration.profile_root, input, output, launch);
  return session.run();
}
function configurationBinding(c: BridgeConfiguration): BridgeBinding {
  return { scope: c.scope, actor: c.actor, viewport: c.viewport, runtime_ms: c.runtime_ms };
}

class Session {
  readonly #id = randomUUID().replaceAll("-", "");
  readonly #reader = new FrameReader();
  #challenge = false;
  #request = 0;
  #callback = 0;
  #pending: Pending | undefined;
  #active: Promise<void> | undefined;
  #responding = false;
  #next: unknown;
  #driver: Driver | undefined;
  #launchAttempted = false;
  #stopped: BridgeExit | undefined;
  #finish!: (exit: BridgeExit) => void;
  #timer: ReturnType<typeof setTimeout> | undefined;
  #outputBytes = 0;
  #write: Promise<void> = Promise.resolve();
  readonly #onData = (chunk: Buffer) => {
    try { this.#reader.push(chunk, (value) => this.receive(value)); }
    catch { this.stop("protocol_error"); }
  };
  readonly #onDisconnect = () => { this.stop("transport_error"); };

  constructor(readonly config: BridgeBinding, readonly profileRoot: string,
    readonly input: Readable, readonly output: Writable, readonly launch: Launch) {}

  async run(): Promise<BridgeExit> {
    const ended = new Promise<BridgeExit>((resolve) => { this.#finish = resolve; });
    this.input.on("data", this.#onData);
    this.input.on("end", this.#onDisconnect);
    this.input.on("close", this.#onDisconnect);
    this.input.on("error", this.#onDisconnect);
    this.output.on("error", this.#onDisconnect);
    this.output.on("close", this.#onDisconnect);
    this.#timer = setTimeout(() => this.stop("deadline"), BRIDGE_LIMITS.handshake_ms);
    const exit = await ended;
    // Driver operations have their own finite deadlines. A late launch is
    // closed in dispatch before becoming visible to any later request.
    let cleanupTimer: ReturnType<typeof setTimeout> | undefined;
    try {
      await Promise.race([
        (async () => { await this.#active; await this.#driver?.close(); await this.#write.catch(() => {}); })(),
        new Promise<void>((resolve) => { cleanupTimer = setTimeout(resolve, 2 * LIMITS.operation_ms); }),
      ]);
    } finally { clearTimeout(cleanupTimer); }
    this.input.off("data", this.#onData);
    this.input.off("end", this.#onDisconnect);
    this.input.off("close", this.#onDisconnect);
    this.input.off("error", this.#onDisconnect);
    // Keep the inert error listener if a failed/stalled write outlives cleanup.
    // There is no retry and this pipe must not be reused for another instance.
    this.output.off("close", this.#onDisconnect);
    this.input.pause();
    return exit;
  }

  stop(exit: BridgeExit): void {
    if (this.#stopped) return;
    this.#stopped = exit;
    clearTimeout(this.#timer);
    this.#pending?.reject();
    this.#pending = undefined;
    this.#finish(exit);
  }

  receive(value: unknown): void {
    if (this.#stopped) return;
    if (!this.#challenge) {
      const hello = record(value, ["version", "kind", "binding_digest"]);
      if (hello.version !== 1 || hello.kind !== "hello" || hello.binding_digest !== bindingDigest(this.config)) return fail();
      this.#challenge = true;
      // Even an idle, never-launched session is finite. Launch delay consumes
      // this outer deadline; the driver additionally enforces its own grant.
      clearTimeout(this.#timer);
      this.#timer = setTimeout(() => this.stop("deadline"), this.config.runtime_ms);
      void this.send({ kind: "challenge", binding_digest: bindingDigest(this.config) }).catch(this.#onDisconnect);
      return;
    }
    if ((value as { kind?: unknown } | null)?.kind === "reply") {
      const v = record(value, ["version", "kind", "session_id", "request_id", "callback_id", "method", "ok", "value"]);
      const pending = this.#pending;
      if (!pending || v.version !== 1 || v.session_id !== this.#id || v.request_id !== this.#request
        || v.callback_id !== pending.id || v.method !== pending.method || typeof v.ok !== "boolean"
        || (!v.ok && v.value !== null)) return fail();
      this.#pending = undefined;
      if (v.ok) pending.resolve(v.value); else pending.reject();
      return;
    }
    const v = record(value, ["version", "kind", "session_id", "request_id", "operation"]);
    if (v.version !== 1 || v.kind !== "request" || v.session_id !== this.#id
      || v.request_id !== this.#request + 1 || this.#request >= BRIDGE_LIMITS.requests) return fail();
    if (this.#active) {
      // A peer may receive our result before Writable's callback runs. Permit
      // exactly one following frame during that final write, never during work.
      if (!this.#responding || this.#next) return fail();
      this.#next = value;
      return;
    }
    const op = operation(v.operation);
    if (op.method === "launch" ? this.#launchAttempted : !this.#driver) return fail();
    this.#request++;
    this.#active = this.dispatch(op).catch(() => this.stop("transport_error"))
      .finally(() => {
        this.#active = undefined;
        this.#responding = false;
        const next = this.#next;
        this.#next = undefined;
        if (next && !this.#stopped) { try { this.receive(next); } catch { this.stop("protocol_error"); } }
      });
  }

  async dispatch(op: Operation): Promise<void> {
    let value: unknown;
    try {
      if (op.method === "launch") {
        this.#launchAttempted = true;
        const driver = await this.launch({ ...this.config, profile_root: this.profileRoot,
          authority: { authorize: (request, signal) => this.rpc("authorize", request, signal) as Promise<Grant> },
          journal: {
            begin: (intent, signal) => this.rpc("begin", intent, signal) as ReturnType<DriverConfiguration["journal"]["begin"]>,
            complete: async (receipt, signal) => { if (await this.rpc("complete", receipt, signal) !== null) return fail(); },
          },
        });
        this.#driver = driver;
        if (this.#stopped) { await driver.close(); return; }
        value = { driver_state: driver.state, browser_version: driver.browser_version };
      } else {
        const driver = this.#driver!;
        switch (op.method) {
          case "pages": value = await driver.pages(op.actor); break;
          case "observe": {
            const observation = await driver.observe(op.actor, op.page_id);
            const { screenshot, ...rest } = observation;
            value = { ...rest, screenshot_base64: screenshot.toString("base64") }; break;
          }
          case "drain": value = await driver.drain(op.control_epoch); break;
          case "act": value = await driver.act(op.actor, op.action_id, op.action); break;
          case "close": await driver.close(); value = { driver_state: driver.state, physical_termination: false }; break;
        }
      }
      if (!this.#stopped) {
        this.#responding = true;
        await this.send({ kind: "result", request_id: this.#request, ok: true, value });
      }
      if (op.method === "close") this.stop("closed");
    } catch (error) {
      if (!this.#stopped) {
        // Only allow-listed driver codes cross the boundary, never Playwright
        // errors, filesystem paths, DOM text, tokens or exception messages.
        this.#responding = true;
        await this.send({ kind: "result", request_id: this.#request, ok: false,
          value: { code: error instanceof DriverError ? error.code : "bridge_failure" } });
        // A failed launch cannot be retried under the same instance identity.
        if (op.method === "launch") this.stop("protocol_error");
      }
    }
  }

  async rpc(method: CallbackMethod, payload: AuthorizationRequest | ActionIntent | ActionReceipt, signal: AbortSignal): Promise<unknown> {
    if (this.#stopped || this.#pending || signal.aborted) return fail();
    const id = ++this.#callback;
    let abort!: () => void;
    const result = new Promise<unknown>((resolve, reject) => {
      this.#pending = { id, method, resolve, reject: () => reject(new DriverError(
        method === "authorize" ? "authority_unavailable" : "journal_unavailable")) };
      abort = () => this.stop("deadline");
      signal.addEventListener("abort", abort, { once: true });
    });
    // Observe a rejected callback while a backpressured write is outstanding.
    void result.catch(() => {});
    try {
      await this.send({ kind: "callback", request_id: this.#request, callback_id: id, method, payload });
      return await result;
    } finally { signal.removeEventListener("abort", abort); }
  }

  send(value: object): Promise<void> {
    if (this.#stopped) return Promise.reject(new Error("bridge_closed"));
    const bytes = Buffer.from(`${JSON.stringify({ version: 1, session_id: this.#id, ...value })}\n`);
    if (bytes.length > BRIDGE_LIMITS.output_frame_bytes || bytes.length > BRIDGE_LIMITS.output_bytes - this.#outputBytes) {
      this.stop("protocol_error"); return Promise.reject(new Error("bridge_limit"));
    }
    this.#outputBytes += bytes.length;
    this.#write = this.#write.then(() => new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("bridge_write_timeout")), BRIDGE_LIMITS.write_ms);
      try {
        this.output.write(bytes, (error) => { clearTimeout(timer); if (error) reject(error); else resolve(); });
      } catch (error) { clearTimeout(timer); reject(error); }
    }));
    return this.#write;
  }
}
