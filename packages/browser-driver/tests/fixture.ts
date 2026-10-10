import { createServer } from "node:http";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { BrowserDriver, ActionIntent, ActionJournal, ActionReceipt, Actor, Authority, AuthorizationRequest, BrowserScope, Grant, Observation } from "../src";

export const actor: Actor = { principal_id: "owner", connection_session_id: "connection" };
export const scope: BrowserScope = { organization_id: "org", computer_id: "computer", computer_generation: 1,
  app_instance_id: "app", app_generation: 1, profile_owner_id: "owner" };

/** Test fixtures only: neither adapter supplies durable production authorization. */
export class TestAuthority implements Authority {
  epoch = 1;
  denied = false;
  validFor = 30000;
  before: ((request: AuthorizationRequest) => Promise<void>) | undefined;
  async authorize(request: AuthorizationRequest): Promise<Grant> {
    await this.before?.(request);
    if (this.denied) throw new Error("revoked");
    return { ...request, control_epoch: this.epoch, valid_for_ms: request.permission === "launch" ? 300000 : this.validFor };
  }
}
export class TestJournal implements ActionJournal {
  intents: ActionIntent[] = [];
  receipts: ActionReceipt[] = [];
  beforeBegin: (() => Promise<void>) | undefined;
  failBegin = false;
  loseComplete = false;
  replay: ActionReceipt | undefined;
  async begin(intent: ActionIntent): Promise<{ kind: "dispatch" } | { kind: "replay"; receipt: ActionReceipt }> {
    this.intents.push(intent);
    await this.beforeBegin?.();
    if (this.failBegin) throw new Error("unavailable");
    return this.replay ? { kind: "replay", receipt: this.replay } : { kind: "dispatch" };
  }
  async complete(receipt: ActionReceipt): Promise<void> {
    this.receipts.push(receipt);
    if (this.loseComplete) throw new Error("ack lost");
  }
}

export async function fixture(runtime_ms = 300000) {
  const root = await mkdtemp(join(tmpdir(), "ac-browser-test-"));
  let effects = 0, mode = "normal", clickNumber = 0, slowReady = false;
  const streams = new Set<import("node:http").ServerResponse>();
  const server = createServer((request, response) => {
    if (request.url === "/events") {
      response.writeHead(200, { "Content-Type": "text/event-stream", "Cache-Control": "no-cache" });
      response.write(": ready\n\n"); streams.add(response);
      response.on("close", () => streams.delete(response)); return;
    }
    if (request.url?.startsWith("/effect")) { effects++; response.end("ok"); return; }
    if (request.url === "/slow-ready") { slowReady = true; response.end("ok"); return; }
    response.setHeader("Content-Type", "text/html");
    response.end(`<!doctype html><title>Browser fixture</title><style>body{margin:20px;font:16px sans-serif}button,input,select{display:block;margin:12px;width:180px;height:30px}</style>
      <button id="button" onclick="this.textContent='clicked';fetch('/effect')">Run action</button>
      <input id="input" aria-label="Name" oninput="document.title=this.value">
      <input type="password" value="fixture-secret" aria-label="Password">
      <select aria-label="Choice" onchange="document.title=this.value"><option>a</option><option>b</option></select>
      <a href="/second" target="_blank">New page</a><div style="height:1500px">End</div>
      <script>new EventSource('/events').onmessage=e=>{
        if(e.data==='move')document.getElementById('button').style.marginLeft='250px';
        if(e.data==='navigate')location.href='/changed';
        if(e.data==='frame'){const f=document.createElement('iframe');document.body.append(f);}
        if(e.data==='slow-geometry'){const b=document.getElementById('button'),original=b.getBoundingClientRect.bind(b);
          b.getBoundingClientRect=()=>{const until=performance.now()+2500;while(performance.now()<until){}return original();};fetch('/slow-ready');}
      };</script>${mode === "large" ? "<button>extra</button>".repeat(3000) : ""}`);
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("no fixture address");
  const url = `http://127.0.0.1:${address.port}`;
  const authority = new TestAuthority(), journal = new TestJournal();
  let driver: BrowserDriver;
  try { driver = await BrowserDriver.launch({ scope, actor, authority, journal, profile_root: root,
    viewport: { width: 800, height: 600 }, runtime_ms }); }
  catch (error) { server.closeAllConnections(); await new Promise<void>((resolve) => server.close(() => resolve())); await rm(root, { recursive: true }); throw error; }
  return { driver, authority, journal, root, url,
    get effects() { return effects; },
    get slowReady() { return slowReady; },
    setMode(value: string) { mode = value; },
    mutate(value: string) { for (const stream of streams) stream.write(`data: ${value}\n\n`); },
    nextId() { return `action_${++clickNumber}`; },
    async navigate(pageId?: string): Promise<Observation> {
      const id = pageId ?? (await driver.pages(actor))[0]!.page_id;
      const before = await driver.observe(actor, id);
      await driver.act(actor, `action_${++clickNumber}`, { kind: "navigate", page_id: id, observation_id: before.observation_id, url });
      return driver.observe(actor, id);
    },
    async close() { await driver.close(); server.closeAllConnections(); await new Promise<void>((resolve) => server.close(() => resolve()));
      if (driver.state !== "closed") throw new Error("browser closure unconfirmed; profile retained");
      await rm(root, { recursive: true }); },
  };
}

export async function eventually(check: () => boolean | Promise<boolean>): Promise<void> {
  const until = Date.now() + 3000;
  while (!await check()) { if (Date.now() >= until) throw new Error("condition timeout"); await new Promise((resolve) => setTimeout(resolve, 20)); }
}
