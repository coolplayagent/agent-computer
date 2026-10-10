import { ElementHandle, Page } from "playwright-core";
import { randomUUID } from "node:crypto";
import { DriverError, ElementObservation, LIMITS, Viewport } from "./contracts";
import { sha256, stableJson } from "./validation";

export interface Metrics {
  width: number; height: number; device_scale: number; scroll_x: number; scroll_y: number;
}
interface Entry {
  tag: string; role: string; label: string; text: string;
  bounds: { x: number; y: number; width: number; height: number };
}
interface DomPayload { entries: Entry[]; truncated: boolean; metrics: Metrics }

/** Renderer content is untrusted, including built-ins a page may have replaced. */
export function parsePayload(value: unknown): DomPayload {
  const fail = (): never => { throw new DriverError("observation_too_large"); };
  if (!value || typeof value !== "object") return fail();
  const v = value as DomPayload;
  if (!Array.isArray(v.entries) || v.entries.length > LIMITS.elements || typeof v.truncated !== "boolean"
    || !v.metrics || typeof v.metrics !== "object") return fail();
  const m = v.metrics;
  if (![m.width, m.height, m.device_scale, m.scroll_x, m.scroll_y].every(Number.isFinite)
    || !Number.isSafeInteger(m.width) || !Number.isSafeInteger(m.height)
    || m.width < 320 || m.width > 1920 || m.height < 240 || m.height > 1080 || m.device_scale !== 1) return fail();
  let bytes = 0;
  const entries = v.entries.map((entry) => {
    if (!entry || typeof entry !== "object" || !entry.bounds || typeof entry.bounds !== "object"
      || typeof entry.tag !== "string" || !/^[a-z][a-z0-9-]{0,63}$/.test(entry.tag)
      || typeof entry.role !== "string" || entry.role.length > 64
      || typeof entry.label !== "string" || entry.label.length > 512
      || typeof entry.text !== "string" || entry.text.length > 512) return fail();
    const b = entry.bounds;
    if (![b.x, b.y, b.width, b.height].every(Number.isFinite) || b.width <= 0 || b.height <= 0
      || b.x >= m.width || b.y >= m.height || b.x + b.width <= 0 || b.y + b.height <= 0) return fail();
    const normalized = Object.freeze({ tag: entry.tag, role: entry.role, label: entry.label, text: entry.text,
      bounds: Object.freeze({ x: b.x, y: b.y, width: b.width, height: b.height }) });
    bytes += Buffer.byteLength(JSON.stringify(normalized));
    if (bytes > LIMITS.dom_bytes) return fail();
    return normalized;
  });
  return { entries, truncated: v.truncated, metrics: Object.freeze({ width: m.width, height: m.height,
    device_scale: m.device_scale, scroll_x: m.scroll_x, scroll_y: m.scroll_y }) };
}
export interface Captured {
  elements: ElementObservation[];
  handles: Map<string, ElementHandle<Element>>;
  focused: ElementHandle<Element> | null;
  metrics: Metrics;
  screenshot: Buffer;
  image_size: Viewport;
  screenshot_sha256: string;
  dom_truncated: boolean;
}

// This function executes in the renderer. It never reads cookies/storage or
// password values, and caps traversal and transfer independently of page size.
function collect(limits: { elements: number; visited_nodes: number; dom_bytes: number }) {
  const elements: Element[] = [], entries: Entry[] = [];
  let visited = 0, bytes = 0, truncated = false;
  const encoder = new TextEncoder();
  const walker = document.createTreeWalker(document.documentElement, NodeFilter.SHOW_ELEMENT);
  let node: Node | null = walker.currentNode;
  while (node) {
    if (++visited > limits.visited_nodes) { truncated = true; break; }
    const element = node as Element;
    const tag = element.tagName.toLowerCase();
    const role = (element.getAttribute("role") ?? "").slice(0, 64);
    const actionable = ["a", "button", "input", "textarea", "select", "summary"].includes(tag)
      || role.length > 0 || element.hasAttribute("contenteditable") || element.hasAttribute("tabindex");
    if (actionable && element.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true })) {
      const rect = element.getBoundingClientRect();
      if (rect.width > 0 && rect.height > 0 && rect.right > 0 && rect.bottom > 0
        && rect.x < innerWidth && rect.y < innerHeight) {
        let content = "";
        // Read a finite number of direct text nodes, not unbounded innerText.
        for (let i = 0; i < Math.min(element.childNodes.length, 16); i++) {
          const child = element.childNodes[i];
          if (child?.nodeType === Node.TEXT_NODE) content += (child.nodeValue ?? "").slice(0, 512 - content.length);
          if (content.length >= 512) break;
        }
        const entry: Entry = {
          tag, role, label: (element.getAttribute("aria-label") ?? element.getAttribute("title") ?? element.getAttribute("placeholder") ?? "").slice(0, 512),
          text: content.trim(), bounds: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
        };
        const size = encoder.encode(JSON.stringify(entry)).length;
        if (entries.length >= limits.elements || bytes + size > limits.dom_bytes) { truncated = true; break; }
        entries.push(entry); elements.push(element); bytes += size;
      }
    }
    node = walker.nextNode();
  }
  return { elements, focused: document.activeElement,
    payload: { entries, truncated,
      metrics: { width: innerWidth, height: innerHeight, device_scale: devicePixelRatio, scroll_x: scrollX, scroll_y: scrollY } },
  };
}

export async function metrics(page: Page): Promise<Metrics> {
  return page.evaluate(() => ({ width: innerWidth, height: innerHeight, device_scale: devicePixelRatio, scroll_x: scrollX, scroll_y: scrollY }));
}

export async function screenshot(page: Page, timeout: number): Promise<{ bytes: Buffer; size: Viewport; sha256: string }> {
  const bytes = await page.screenshot({ type: "png", fullPage: false, scale: "css", timeout });
  if (bytes.length > LIMITS.screenshot_bytes) throw new DriverError("observation_too_large");
  if (bytes.length < 24 || !bytes.subarray(0, 8).equals(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]))) {
    throw new DriverError("browser_unavailable");
  }
  return { bytes, size: Object.freeze({ width: bytes.readUInt32BE(16), height: bytes.readUInt32BE(20) }), sha256: sha256(bytes) };
}

export async function capture(page: Page, timeout: number): Promise<Captured> {
  const result = await page.evaluateHandle(collect, {
    elements: LIMITS.elements, visited_nodes: LIMITS.visited_nodes, dom_bytes: LIMITS.dom_bytes,
  });
  const handles = new Map<string, ElementHandle<Element>>();
  let focused: ElementHandle<Element> | null = null;
  try {
    const payloadHandle = await result.getProperty("payload");
    let payload: DomPayload;
    try { payload = parsePayload(await payloadHandle.jsonValue()); }
    finally { await payloadHandle.dispose(); }
    const elementsHandle = await result.getProperty("elements");
    const properties = await elementsHandle.getProperties();
    await elementsHandle.dispose();
    const elements: ElementObservation[] = [];
    try {
      for (let i = 0; i < payload.entries.length; i++) {
        const handle = properties.get(String(i))?.asElement();
        if (!handle) throw new DriverError("stale_observation");
        const element_id = `element_${randomUUID().replaceAll("-", "")}`;
        handles.set(element_id, handle as ElementHandle<Element>);
        properties.delete(String(i));
        elements.push(Object.freeze({ element_id, ...payload.entries[i]! }));
      }
    } finally { await Promise.allSettled([...properties.values()].map((handle) => handle.dispose())); }
    const focusedHandle = await result.getProperty("focused");
    focused = focusedHandle.asElement() as ElementHandle<Element> | null;
    if (!focused) await focusedHandle.dispose();
    const image = await screenshot(page, timeout);
    if (stableJson(payload.metrics) !== stableJson(await metrics(page))
      || image.size.width !== payload.metrics.width || image.size.height !== payload.metrics.height) {
      throw new DriverError("stale_observation");
    }
    for (const element of elements) {
      if (!await targetUnchanged(handles.get(element.element_id)!, element)) throw new DriverError("stale_observation");
    }
    return { handles, focused, elements, metrics: payload.metrics,
      screenshot: image.bytes, screenshot_sha256: image.sha256, image_size: image.size,
      dom_truncated: payload.truncated };
  } catch (error) {
    await disposeHandles(handles, focused);
    throw error;
  } finally { await result.dispose(); }
}

export async function targetUnchanged(handle: ElementHandle<Element>, entry: ElementObservation): Promise<boolean> {
  return handle.evaluate((element, expected) => {
    if (!element.isConnected || !element.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true })) return false;
    const rect = element.getBoundingClientRect();
    let content = "";
    for (let i = 0; i < Math.min(element.childNodes.length, 16); i++) {
      const child = element.childNodes[i];
      if (child?.nodeType === Node.TEXT_NODE) content += (child.nodeValue ?? "").slice(0, 512 - content.length);
      if (content.length >= 512) break;
    }
    return rect.x === expected.bounds.x && rect.y === expected.bounds.y
      && rect.width === expected.bounds.width && rect.height === expected.bounds.height
      && element.tagName.toLowerCase() === expected.tag
      && (element.getAttribute("role") ?? "").slice(0, 64) === expected.role
      && (element.getAttribute("aria-label") ?? element.getAttribute("title") ?? element.getAttribute("placeholder") ?? "").slice(0, 512) === expected.label
      && content.trim() === expected.text;
  }, entry);
}

export async function disposeHandles(handles: Map<string, ElementHandle<Element>>, focused: ElementHandle<Element> | null): Promise<void> {
  await Promise.allSettled([...handles.values(), ...(focused ? [focused] : [])].map((handle) => handle.dispose()));
  handles.clear();
}
