import { performance } from "node:perf_hooks";
import { Actor, Authority, BrowserScope, DriverError, Grant, LIMITS } from "./contracts";
import { stableJson } from "./validation";

/** Relative grants cannot recover time spent waiting for the authority. */
export interface LiveGrant { readonly epoch: number; readonly until: number }

export async function authorize(
  authority: Authority, scope: BrowserScope, actor: Actor, permission: "launch" | "observe" | "control",
  deadline: number, signal: AbortSignal,
): Promise<LiveGrant> {
  const started = performance.now();
  let grant: Grant;
  try { grant = await authority.authorize(Object.freeze({ scope, actor, permission }), signal); }
  catch { throw new DriverError("authority_unavailable"); }
  if (signal.aborted || !grant || stableJson(grant.scope) !== stableJson(scope)
    || stableJson(grant.actor) !== stableJson(actor) || grant.permission !== permission
    || !Number.isSafeInteger(grant.control_epoch) || grant.control_epoch < (permission === "control" ? 1 : 0)
    || !Number.isSafeInteger(grant.valid_for_ms) || grant.valid_for_ms < 1
    || grant.valid_for_ms > (permission === "launch" ? LIMITS.runtime_ms : LIMITS.authority_ms)) {
    throw new DriverError("authority_mismatch");
  }
  const until = Math.min(deadline, started + grant.valid_for_ms);
  if (performance.now() >= until) throw new DriverError("authority_unavailable");
  return Object.freeze({ epoch: grant.control_epoch, until });
}
