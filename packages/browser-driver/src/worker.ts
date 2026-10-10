#!/usr/bin/env node
import { resolve } from "node:path";
import { lstat, mkdir, realpath } from "node:fs/promises";
import { binding, BRIDGE_LIMITS } from "./bridge-protocol";
import { serveBridge } from "./bridge";

/** Operator image entrypoint. argv carries a persisted binding, never secrets.
 * The private profile root is a fixed image mount, not a wire-selected path.
 */
async function main(): Promise<number> {
  if (process.argv.length === 3 && process.argv[2] === "--prepare-profile-volume") {
    // The approved init container has a fresh memory emptyDir at this fixed
    // path. Never reuse an existing leaf or accept a path/identity from stdin.
    const volume = "/browser-volume";
    const metadata = await lstat(volume);
    if (!metadata.isDirectory() || metadata.isSymbolicLink() || await realpath(volume) !== volume
      || process.getuid?.() !== 1000 || process.getgid?.() !== 1000) return 1;
    await mkdir(`${volume}/private`, { mode: 0o700 });
    const privateRoot = await lstat(`${volume}/private`);
    if (!privateRoot.isDirectory() || privateRoot.isSymbolicLink() || privateRoot.uid !== 1000
      || privateRoot.gid !== 1000 || (privateRoot.mode & 0o777) !== 0o700) return 1;
    return 0;
  }
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
