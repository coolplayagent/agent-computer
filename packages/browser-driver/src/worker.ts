#!/usr/bin/env node
import { resolve } from "node:path";
import { binding, BRIDGE_LIMITS } from "./bridge-protocol";
import { serveBridge } from "./bridge";

/** Operator image entrypoint. argv carries a persisted binding, never secrets.
 * The private profile root is a fixed image mount, not a wire-selected path.
 */
async function main(): Promise<number> {
  if (process.argv.length !== 4 || process.argv[2] !== "--stdio-binding-json"
    || Buffer.byteLength(process.argv[3]!) > BRIDGE_LIMITS.input_frame_bytes) return 2;
  const config = binding(JSON.parse(process.argv[3]!));
  const exit = await serveBridge({ ...config, profile_root: resolve("/browser-state") }, process.stdin, process.stdout);
  return exit === "closed" ? 0 : 1;
}

if (require.main === module) {
  void main().then((code) => { process.exit(code); }, () => {
    process.stderr.write("browser worker failed\n", () => process.exit(1));
  });
}
