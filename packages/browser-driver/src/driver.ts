import { chromium, BrowserContext, ElementHandle, Page } from "playwright-core";
import { randomUUID } from "node:crypto";
import { lstat, mkdtemp, realpath } from "node:fs/promises";
import { join, resolve } from "node:path";
import { performance } from "node:perf_hooks";
import {
  Action, ActionIntent, ActionJournal, ActionReceipt, ActionResult, Actor, Authority,
  BrowserScope, DriverError, LIMITS, Observation, Viewport,
} from "./contracts";
import { authorize, LiveGrant } from "./authority";
import { capture, Captured, disposeHandles, metrics, screenshot, targetUnchanged } from "./observation";
import * as validate from "./validation";

export interface DriverConfiguration {
  readonly scope: BrowserScope;
  readonly actor: Actor;
  readonly authority: Authority;
  readonly journal: ActionJournal;
  /** Trusted worker-allocated private local directory, never an HTTP path field. */
  readonly profile_root: string;
  readonly viewport: Viewport;
  readonly runtime_ms: number;
}

interface PageState { readonly id: string; readonly page: Page; generation: number }
interface CachedObservation {
  readonly value: Observation;
  readonly captured: Captured;
  readonly until: number;
}
interface Operation { readonly deadline: number; readonly signal: AbortSignal }

/** Internal Browser App adapter. Public authentication/lifecycle adapters are separate. */
export class BrowserDriver {
  readonly scope: BrowserScope;
  readonly profile_id: string;
  readonly browser_version: string;
  #context: BrowserContext;
  #authority: Authority;
  #journal: ActionJournal;
  #viewport: Viewport;
  #until: number;
  #timer: ReturnType<typeof setTimeout>;
  #pages = new Map<string, PageState>();
  #pageIds = new WeakMap<Page, string>();
  #observations = new Map<string, CachedObservation>();
  #actions = new Map<string, { digest: string; result?: ActionResult }>();
  #epoch = 0;
  #busy = false;
  #blocked = false;
  #closing: Promise<void> | undefined;
  #state: "open" | "closing" | "closed" | "close_unknown" = "open";

  private constructor(config: DriverConfiguration, context: BrowserContext, profileId: string, until: number, epoch: number) {
    this.scope = config.scope;
    this.profile_id = profileId;
    this.#context = context;
    this.#authority = config.authority;
    this.#journal = config.journal;
    this.#viewport = config.viewport;
    this.#until = until;
    this.#epoch = epoch;
    this.browser_version = context.browser()?.version() ?? "unavailable";
    context.on("page", (page) => this.#register(page));
    context.on("close", () => { if (this.#state === "open") { this.#blocked = true; void this.close(); } });
    for (const page of context.pages()) this.#register(page);
    this.#timer = setTimeout(() => { void this.close(); }, Math.max(1, until - performance.now()));
    this.#timer.unref();
  }

  static async launch(configuration: DriverConfiguration): Promise<BrowserDriver> {
    const config = Object.freeze({ ...configuration, scope: validate.scope(configuration.scope),
      actor: validate.actor(configuration.actor), viewport: validate.viewport(configuration.viewport),
      runtime_ms: validate.runtimeLimit(configuration.runtime_ms) });
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), LIMITS.operation_ms);
    let grant: LiveGrant;
    try {
      grant = await deadline(authorize(config.authority, config.scope, config.actor, "launch",
        performance.now() + config.runtime_ms, controller.signal), LIMITS.operation_ms,
      () => controller.abort(), "authority_unavailable");
    } finally { clearTimeout(timer); }
    const root = resolve(config.profile_root);
    let profileId: string;
    try {
      const metadata = await lstat(root);
      if (!metadata.isDirectory() || metadata.isSymbolicLink() || await realpath(root) !== root
        || (process.platform !== "win32" && ((metadata.mode & 0o077) !== 0 || metadata.uid !== process.getuid?.()))) {
        throw new DriverError("invalid_request");
      }
      profileId = (await mkdtemp(join(root, "profile_"))).slice(root.length + 1);
    } catch { throw new DriverError("invalid_request"); }
    if (performance.now() >= grant.until) throw new DriverError("authority_unavailable");
    let context: BrowserContext;
    try {
      context = await chromium.launchPersistentContext(join(root, profileId), {
        channel: "chromium", headless: true, chromiumSandbox: true,
        viewport: config.viewport, deviceScaleFactor: 1, acceptDownloads: false,
        serviceWorkers: "block", permissions: [],
        timeout: Math.min(LIMITS.operation_ms, grant.until - performance.now()),
        env: { PATH: process.env.PATH ?? "/usr/bin:/bin", LANG: "C.UTF-8", HOME: join(root, profileId) },
      });
    } catch { throw new DriverError("browser_unavailable"); }
    if (performance.now() >= grant.until) { await context.close(); throw new DriverError("authority_unavailable"); }
    return new BrowserDriver(config, context, profileId, grant.until, grant.epoch);
  }

  get state(): string { return this.#state; }
  get control_blocked(): boolean { return this.#blocked; }

  async pages(actor: Actor): Promise<readonly { page_id: string; document_generation: number; url: string; title: string }[]> {
    const caller = validate.actor(actor);
    return this.#operation(async (op) => {
      const grant = await this.#grant(caller, "observe", op);
      const result = [];
      for (const state of this.#pages.values()) {
        result.push(Object.freeze({ page_id: state.id, document_generation: state.generation,
          url: state.page.url().slice(0, 4096), title: (await state.page.title()).slice(0, 1024) }));
      }
      await this.#checkRead(caller, grant, op);
      return Object.freeze(result);
    });
  }

  async observe(actor: Actor, pageId: string): Promise<Observation> {
    const caller = validate.actor(actor), id = validate.identifier(pageId);
    return this.#operation(async (op) => {
      const grant = await this.#grant(caller, "observe", op), state = this.#page(id);
      const generation = state.generation;
      const capturedAt = new Date().toISOString(), started = performance.now();
      const captured = await capture(state.page, remaining(grant.until));
      try {
        const renewed = await this.#checkRead(caller, grant, op);
        if (state.generation !== generation || state.page.isClosed()
          || captured.metrics.width !== this.#viewport.width || captured.metrics.height !== this.#viewport.height
          || captured.metrics.device_scale !== 1) throw new DriverError("stale_observation");
        const until = Math.min(started + LIMITS.observation_ms, renewed.until, this.#until);
        const observation_id = `observation_${randomUUID().replaceAll("-", "")}`;
        const value: Observation = Object.freeze({ observation_id, scope: this.scope, page_id: id,
          document_generation: generation, control_epoch: grant.epoch, captured_at: capturedAt,
          valid_for_ms: Math.max(0, Math.floor(until - performance.now())), viewport: this.#viewport,
          image_size: captured.image_size, device_scale: captured.metrics.device_scale,
          coordinate_space: "css_viewport", image_scale: 1,
          screenshot_sha256: captured.screenshot_sha256, screenshot: captured.screenshot,
          elements: Object.freeze(captured.elements), dom_truncated: captured.dom_truncated });
        while (this.#observations.size >= LIMITS.observations) {
          const oldest = this.#observations.keys().next().value;
          if (oldest) await this.#forget(oldest);
        }
        this.#observations.set(observation_id, { value, captured, until });
        return value;
      } catch (error) { await disposeHandles(captured.handles, captured.focused); throw error; }
    });
  }

  async act(actor: Actor, actionId: string, input: unknown): Promise<ActionResult> {
    const caller = validate.actor(actor), id = validate.identifier(actionId), action = validate.action(input);
    return this.#operation(async (op) => {
      const grant = await this.#grant(caller, "control", op);
      const intent: ActionIntent = Object.freeze({ scope: this.scope, actor: caller,
        control_epoch: grant.epoch, action_id: id, action,
        input_digest: validate.digest({ scope: this.scope, actor: caller, control_epoch: grant.epoch, action }) });
      const prior = this.#actions.get(id);
      if (prior) {
        if (prior.digest !== intent.input_digest) throw new DriverError("invalid_request");
        if (prior.result) return prior.result;
        throw new DriverError("control_blocked");
      }
      if (this.#blocked) throw new DriverError("control_blocked");
      // Bound unresolved/completed action memory for this finite instance.
      if (this.#actions.size >= 1024) throw new DriverError("control_blocked");
      await this.#validateTarget(action, grant);
      this.#assertLive(grant.until);
      this.#actions.set(id, { digest: intent.input_digest });
      let admission: Awaited<ReturnType<ActionJournal["begin"]>>;
      try { admission = await this.#journal.begin(intent, op.signal); }
      catch { this.#blocked = true; throw new DriverError("journal_unavailable"); }
      this.#assertLive(op.deadline);
      if (!admission || !["dispatch", "replay"].includes(admission.kind)) {
        this.#blocked = true; throw new DriverError("journal_unavailable");
      }
      if (admission.kind === "replay") {
        const receipt = this.#validateReceipt(admission.receipt, intent);
        const result = Object.freeze({ receipt, recorded: true });
        this.#actions.set(id, { digest: intent.input_digest, result });
        if (result.receipt.outcome === "unknown") this.#blocked = true;
        return result;
      }
      let outcome: "delivered" | "unknown" = "unknown", pageId: string | undefined;
      try {
        // Recheck authority after the committed intent, immediately before effects.
        const current = await this.#grant(caller, "control", op);
        if (current.epoch !== grant.epoch) throw new DriverError("stale_observation");
        const target = await this.#validateTarget(action, current);
        this.#assertLive(current.until);
        pageId = await deadline(this.#dispatch(action, target, current), remaining(current.until),
          () => { this.#blocked = true; void this.close(); }, "browser_unavailable");
        outcome = "delivered";
      } catch { this.#blocked = true; }
      await this.#invalidate();
      const receipt: ActionReceipt = Object.freeze({ action_id: id, input_digest: intent.input_digest,
        app_instance_id: this.scope.app_instance_id, app_generation: this.scope.app_generation,
        control_epoch: grant.epoch, outcome, ...(pageId ? { page_id: pageId } : {}) });
      let recorded = false;
      try { await this.#journal.complete(receipt, op.signal); recorded = !op.signal.aborted; }
      catch { this.#blocked = true; }
      if (!recorded) this.#blocked = true;
      // If completion acknowledgement is lost, do not overwrite a possibly
      // committed receipt with a second write or repeat the browser action.
      const result: ActionResult = Object.freeze({
        receipt: recorded ? receipt : Object.freeze({ ...receipt, outcome: "unknown" }), recorded,
      });
      this.#actions.set(id, { digest: intent.input_digest, result });
      return result;
    });
  }

  /** Private controller barrier. A failed/uncertain action cannot be drained into
   * a new owner. All admitted inputs are discrete; release inputs explicitly and
   * discard observations before acknowledging a serial handoff. */
  async drain(epoch: number): Promise<{ control_epoch: number; drained: true; requires_observation: true }> {
    if (!Number.isSafeInteger(epoch) || epoch < 1 || epoch < this.#epoch) throw new DriverError("invalid_request");
    return this.#operation(async (op) => {
      if (this.#blocked) throw new DriverError("control_blocked");
      for (const { page } of this.#pages.values()) {
        if (page.isClosed()) continue;
        for (const button of ["left", "middle", "right"] as const) await page.mouse.up({ button });
        for (const key of ["Enter", "Tab", "Escape", "Backspace", "Delete", "ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End", "PageUp", "PageDown", "Space"]) await page.keyboard.up(key);
      }
      await this.#invalidate();
      this.#assertLive(op.deadline);
      this.#epoch = epoch;
      return { control_epoch: epoch, drained: true, requires_observation: true };
    });
  }

  async close(): Promise<void> {
    if (this.#closing) return this.#closing;
    this.#state = "closing";
    clearTimeout(this.#timer);
    this.#closing = (async () => {
      try {
        await deadline(this.#context.close(), LIMITS.operation_ms, () => {}, "browser_unavailable");
        this.#state = "closed";
      } catch { this.#state = "close_unknown"; }
      this.#observations.clear();
      this.#pages.clear();
    })();
    return this.#closing;
  }

  #register(page: Page): void {
    if (this.#pageIds.has(page)) return;
    if (this.#state !== "open" || this.#pages.size >= LIMITS.pages) {
      void page.close().catch(() => { this.#blocked = true; void this.close(); });
      return;
    }
    const id = `page_${randomUUID().replaceAll("-", "")}`;
    const state: PageState = { id, page, generation: 1 };
    this.#pageIds.set(page, id); this.#pages.set(id, state);
    const changed = () => { state.generation++; void this.#invalidate(id); };
    page.on("framenavigated", changed); page.on("frameattached", changed); page.on("framedetached", changed);
    page.on("close", () => { this.#pages.delete(id); changed(); });
    page.on("crash", () => { this.#blocked = true; changed(); });
    page.on("dialog", (dialog) => { void dialog.dismiss().catch(() => {}); });
    page.on("download", (download) => { void download.cancel().catch(() => {}); });
  }

  #page(id: string): PageState {
    const page = this.#pages.get(id);
    if (!page || page.page.isClosed()) throw new DriverError("page_unavailable");
    return page;
  }

  async #grant(actor: Actor, permission: "observe" | "control", op: Operation): Promise<LiveGrant> {
    this.#assertLive(op.deadline);
    const grant = await authorize(this.#authority, this.scope, actor, permission, op.deadline, op.signal);
    if (grant.epoch < this.#epoch) throw new DriverError("authority_mismatch");
    if (grant.epoch > this.#epoch) { await this.#invalidate(); this.#epoch = grant.epoch; }
    this.#assertLive(grant.until);
    return grant;
  }

  async #checkRead(actor: Actor, original: LiveGrant, op: Operation): Promise<LiveGrant> {
    const current = await this.#grant(actor, "observe", op);
    if (original.epoch !== current.epoch) throw new DriverError("stale_observation");
    return current;
  }

  async #validateTarget(action: Action, grant: LiveGrant): Promise<ElementHandle<Element> | undefined> {
    if (action.kind === "create_page") {
      if (this.#pages.size >= LIMITS.pages) throw new DriverError("page_limit");
      return undefined;
    }
    const state = this.#page(action.page_id), cached = this.#observations.get(action.observation_id);
    if (!cached || cached.until <= performance.now() || cached.value.page_id !== state.id
      || cached.value.document_generation !== state.generation || cached.value.control_epoch !== grant.epoch
      || validate.stableJson(await metrics(state.page)) !== validate.stableJson(cached.captured.metrics)) {
      throw new DriverError("stale_observation");
    }
    if ("element_id" in action) {
      const handle = cached.captured.handles.get(action.element_id);
      const entry = cached.value.elements.find((e) => e.element_id === action.element_id);
      if (!handle || !entry || !await targetUnchanged(handle, entry)) throw new DriverError("stale_observation");
      this.#checkObservation(state, cached, action.observation_id, grant);
      return handle;
    }
    if (action.kind === "click_at") {
      if (action.x >= this.#viewport.width || action.y >= this.#viewport.height
        || (await screenshot(state.page, remaining(grant.until))).sha256 !== cached.value.screenshot_sha256) {
        throw new DriverError("stale_observation");
      }
    }
    if (action.kind === "key" && (!cached.captured.focused
      || !await cached.captured.focused.evaluate((element) => element === document.activeElement))) {
      throw new DriverError("stale_observation");
    }
    this.#checkObservation(state, cached, action.observation_id, grant);
    return undefined;
  }

  #checkObservation(state: PageState, cached: CachedObservation, id: string, grant: LiveGrant): void {
    // Proof collection is asynchronous: it cannot extend the original observation.
    if (this.#observations.get(id) !== cached || cached.until <= performance.now()
      || cached.value.document_generation !== state.generation || state.page.isClosed()
      || cached.value.control_epoch !== grant.epoch) throw new DriverError("stale_observation");
    this.#assertLive(grant.until);
  }

  async #dispatch(action: Action, target: ElementHandle<Element> | undefined, grant: LiveGrant): Promise<string> {
    if (action.kind === "create_page") {
      const page = await this.#context.newPage();
      this.#register(page);
      const id = this.#pageIds.get(page);
      if (!id) throw new DriverError("page_limit");
      return id;
    }
    const page = this.#page(action.page_id).page, timeout = remaining(grant.until);
    switch (action.kind) {
      case "close_page": await page.close({ runBeforeUnload: false }); break;
      case "navigate": await page.goto(action.url, { waitUntil: "domcontentloaded", timeout }); break;
      case "click": await target!.click({ timeout }); break;
      case "click_at": await page.mouse.click(action.x, action.y); break;
      case "type": await target!.fill(action.text, { timeout }); break;
      case "select": await target!.selectOption(action.value, { timeout }); break;
      case "key": await page.keyboard.press(action.key); break;
      case "scroll": await page.mouse.wheel(action.delta_x, action.delta_y); break;
    }
    return action.page_id;
  }

  #validateReceipt(receipt: ActionReceipt, intent: ActionIntent): ActionReceipt {
    if (!receipt || receipt.action_id !== intent.action_id || receipt.input_digest !== intent.input_digest
      || receipt.app_instance_id !== this.scope.app_instance_id || receipt.app_generation !== this.scope.app_generation
      || receipt.control_epoch !== intent.control_epoch || !["delivered", "unknown"].includes(receipt.outcome)
      || (receipt.page_id !== undefined && (typeof receipt.page_id !== "string" || !/^[a-zA-Z0-9_-]{1,128}$/.test(receipt.page_id)))) {
      this.#blocked = true; throw new DriverError("journal_unavailable");
    }
    return Object.freeze({ action_id: receipt.action_id, input_digest: receipt.input_digest,
      app_instance_id: receipt.app_instance_id, app_generation: receipt.app_generation,
      control_epoch: receipt.control_epoch, outcome: receipt.outcome,
      ...(receipt.page_id ? { page_id: receipt.page_id } : {}) });
  }

  async #forget(id: string): Promise<void> {
    const value = this.#observations.get(id); this.#observations.delete(id);
    if (value) await disposeHandles(value.captured.handles, value.captured.focused);
  }

  async #invalidate(pageId?: string): Promise<void> {
    const ids = [...this.#observations].filter(([, value]) => !pageId || value.value.page_id === pageId).map(([id]) => id);
    await Promise.all(ids.map((id) => this.#forget(id)));
  }

  #assertLive(until: number): void {
    if (this.#state !== "open" || performance.now() >= Math.min(until, this.#until)) throw new DriverError("closed");
  }

  async #operation<T>(run: (op: Operation) => Promise<T>): Promise<T> {
    this.#assertLive(this.#until);
    if (this.#busy) throw new DriverError("busy");
    this.#busy = true;
    const controller = new AbortController(), until = Math.min(this.#until, performance.now() + LIMITS.operation_ms);
    try {
      return await deadline(run({ deadline: until, signal: controller.signal }), remaining(until), () => {
        controller.abort(); this.#blocked = true; void this.close();
      }, "browser_unavailable");
    } catch (error) { throw error instanceof DriverError ? error : new DriverError("browser_unavailable"); }
    finally { this.#busy = false; }
  }
}

function remaining(until: number): number {
  const left = Math.floor(until - performance.now());
  if (left <= 0) throw new DriverError("closed");
  return left;
}

async function deadline<T>(promise: Promise<T>, milliseconds: number, expire: () => void, code: "authority_unavailable" | "browser_unavailable"): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([promise, new Promise<never>((_, reject) => {
      timer = setTimeout(() => { expire(); reject(new DriverError(code)); }, Math.max(1, milliseconds));
    })]);
  } finally { if (timer) clearTimeout(timer); }
}
