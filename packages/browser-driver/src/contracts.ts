/** Internal worker interface. An HTTP caller cannot supply these trusted adapters. */
export interface BrowserScope {
  readonly organization_id: string;
  readonly computer_id: string;
  readonly computer_generation: number;
  readonly app_instance_id: string;
  readonly app_generation: number;
  readonly profile_owner_id: string;
}

export interface Actor {
  readonly principal_id: string;
  readonly connection_session_id: string;
}

export interface AuthorizationRequest {
  readonly scope: BrowserScope;
  readonly actor: Actor;
  readonly permission: "launch" | "observe" | "control";
}

export interface Grant extends AuthorizationRequest {
  /** Current shared Browser control epoch, including for observations. */
  readonly control_epoch: number;
  /** At most 30 seconds for reads/actions, or 5 minutes for admitted runtime. */
  readonly valid_for_ms: number;
}

export interface Authority {
  /** Check identity, grants, App/profile ACL and generation; launch needs admitted activation/budget. */
  authorize(request: AuthorizationRequest, signal: AbortSignal): Promise<Grant>;
}

export type Action =
  | { readonly kind: "create_page" }
  | { readonly kind: "close_page"; readonly page_id: string; readonly observation_id: string }
  | { readonly kind: "navigate"; readonly page_id: string; readonly observation_id: string; readonly url: string }
  | { readonly kind: "click"; readonly page_id: string; readonly observation_id: string; readonly element_id: string }
  | { readonly kind: "click_at"; readonly page_id: string; readonly observation_id: string; readonly x: number; readonly y: number }
  | { readonly kind: "type"; readonly page_id: string; readonly observation_id: string; readonly element_id: string; readonly text: string }
  | { readonly kind: "select"; readonly page_id: string; readonly observation_id: string; readonly element_id: string; readonly value: string }
  | { readonly kind: "key"; readonly page_id: string; readonly observation_id: string; readonly key: string }
  | { readonly kind: "scroll"; readonly page_id: string; readonly observation_id: string; readonly delta_x: number; readonly delta_y: number };

export interface ActionIntent {
  readonly scope: BrowserScope;
  readonly actor: Actor;
  readonly control_epoch: number;
  readonly action_id: string;
  readonly input_digest: string;
  readonly action: Action;
}

export interface ActionReceipt {
  readonly action_id: string;
  readonly input_digest: string;
  readonly app_instance_id: string;
  readonly app_generation: number;
  readonly control_epoch: number;
  readonly outcome: "delivered" | "unknown";
  readonly page_id?: string;
}

export interface ActionResult {
  readonly receipt: ActionReceipt;
  /** False means result persistence was not acknowledged; never report success. */
  readonly recorded: boolean;
}

export interface ActionJournal {
  /** Commit a unique intent before effects. Existing unresolved intent MUST NOT redispatch. */
  begin(intent: ActionIntent, signal: AbortSignal): Promise<
    { readonly kind: "dispatch" } | { readonly kind: "replay"; readonly receipt: ActionReceipt }
  >;
  /** Commit once; an uncertain acknowledgement must be reconciled, not overwritten. */
  complete(receipt: ActionReceipt, signal: AbortSignal): Promise<void>;
}

export interface Viewport { readonly width: number; readonly height: number }
export interface Rectangle { readonly x: number; readonly y: number; readonly width: number; readonly height: number }
export interface ElementObservation {
  readonly element_id: string;
  readonly tag: string;
  readonly role: string;
  readonly label: string;
  readonly text: string;
  readonly bounds: Rectangle;
}

export interface Observation {
  readonly observation_id: string;
  readonly scope: BrowserScope;
  readonly page_id: string;
  readonly document_generation: number;
  readonly control_epoch: number;
  readonly captured_at: string;
  readonly valid_for_ms: number;
  readonly viewport: Viewport;
  readonly image_size: Viewport;
  readonly device_scale: number;
  readonly coordinate_space: "css_viewport";
  readonly image_scale: 1;
  readonly screenshot_sha256: string;
  readonly screenshot: Buffer;
  readonly elements: readonly ElementObservation[];
  readonly dom_truncated: boolean;
}

export type DriverErrorCode = "invalid_request" | "authority_unavailable" | "authority_mismatch"
  | "busy" | "closed" | "control_blocked" | "stale_observation" | "page_unavailable"
  | "page_limit" | "observation_too_large" | "browser_unavailable" | "journal_unavailable";

export class DriverError extends Error {
  constructor(readonly code: DriverErrorCode) {
    // Raw Playwright errors may contain private URLs, typed values and DOM text.
    super(code);
    this.name = "DriverError";
  }
}

export const LIMITS = Object.freeze({
  pages: 8, observations: 16, elements: 100, visited_nodes: 2000,
  dom_bytes: 32768, screenshot_bytes: 2 * 1024 * 1024,
  observation_ms: 30000, authority_ms: 30000, operation_ms: 10000,
  runtime_ms: 300000, text_bytes: 16384,
});
