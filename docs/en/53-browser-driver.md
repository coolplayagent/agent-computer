# 53. Internal Browser Driver

`packages/browser-driver` implements the TypeScript/Playwright component from D09: one private Chromium profile per admitted Browser App instance, multiple pages, bounded observations and explicit actions. It is an internal worker library. Public Browser API/CLI commands, App launch integration, GUI control leases and ComputerView are still pending; the service continues to advertise Browser as unsupported.

## Worker contract

`BrowserDriver.launch` requires a trusted `Authority`, `ActionJournal`, private local `profile_root`, viewport, runtime budget, actor and exact organization/Computer/App generation/profile-owner scope. These are worker inputs, not client-supplied callbacks or filesystem paths. Authority must check current identity, connection, grants, App/profile ACLs, generation and shared control epoch; launch additionally requires durable activation and resource admission. The journal must atomically reserve each scoped action ID before dispatch. A duplicate unresolved intent must never return permission to dispatch again. Test adapters intentionally do not supply these production guarantees.

The worker creates a new private profile directory and starts matching Chromium with its sandbox enabled and no ambient credentials in the browser environment. It cannot attach a user's personal profile, accept arbitrary Chromium arguments, expose CDP/evaluation, or mount a shared Candidate as its profile. The external App sandbox still needs network policy, CPU/memory/PID/disk limits, credentials isolation and independent runtime fencing. HTTP(S) navigation validation is not an egress or SSRF policy. Pages may load subresources, execute scripts and create tabs. Renderer-derived DOM metadata is untrusted page content and never authorization evidence. Node validates its shape and limits again; hostile renderer behavior still requires the external process/resource boundary.

## Observe and act

`pages(actor)` and `observe(actor, page_id)` perform online read checks before and after browser reads. An observation carries scope, random page and observation IDs, document generation, control epoch, capture time, remaining validity, CSS viewport, PNG dimensions, device scale, image scale and SHA-256. Images use CSS viewport coordinates and scale 1. Screenshot bytes are bounded to 2 MiB; traversal visits at most 2,000 elements and returns at most 100 actionable element references with 32 KiB of renderer metadata. Opaque element references retain actual handles; no client selectors are accepted. Password values, cookies and storage are not collected by the DOM walk. Visible pixels and labels can still contain sensitive page content and need the caller's read permission.

Actions are `create_page`, `close_page`, `navigate`, `click`, `click_at`, `type`, `select`, `key` and `scroll`. Apart from page creation, each requires a current observation for its page. DOM actions revalidate the referenced element's identity, visibility, bounds and bounded description. Coordinate clicks additionally require identical fresh screenshot bytes. Keys require the same focused element; key combinations outside the finite allowlist are rejected. No operation accepts arbitrary script. Revalidation cannot freeze a page between checking and physical input; a fresh observation and `delivered` receipt do not prove the intended business effect. Navigation/frame changes, control epoch changes and dispatched actions invalidate observations; changed viewport/scroll metrics, target geometry and expired references reject before new intent admission. The observation lifetime is at most 30 seconds and is also bounded by the actual grant and operation deadline, so current observations usually expire sooner.

One operation runs at a time. Limits include eight pages, 16 observations, 1,024 action records per instance, 10 seconds per operation, 16 KiB typed text and a five-minute admitted runtime ceiling. Extra page-created tabs are closed; this bounds retained pages, not network requests already made by the page. Expiry initiates context closure. Closure reports `closed` or `close_unknown`; even `closed` is not a node/cgroup fence or authorization to transfer profile ownership. The profile remains private for the later trusted capture/encryption/checkpoint path. This increment does not restore or publish profiles.

## Delivery uncertainty

`act(actor, action_id, action)` binds its digest to scope, actor, control epoch and normalized action. The driver validates the observation, persists an intent, rechecks current authority and the target, then dispatches once. A verified browser acknowledgement yields `delivered`; this is not proof of business effect. Failure after intent admission yields `unknown` and blocks new control. Failure to acknowledge result persistence returns `recorded=false` and `unknown`, without overwriting a possibly committed receipt. Identical local retries return the same result while authorized; changed input under the same ID is rejected. Unknown never triggers automatic replay.

An operation timeout closes the context and can return an error without a receipt. Once an action has been admitted, the caller must treat such loss as uncertain and reconcile the durable journal. There is no cross-process journal recovery API here. The future adapter must reconcile exact action identity and original physical runtime; restarting the library or using a new action ID cannot establish that the old effect did not happen. Observations remain available after an uncertain completion if the context is still open and read authority permits them.

## Build and validation

Node 22.22.1, TypeScript 5.9.3, pnpm 9.15.9 and Playwright Core 1.64.0 are pinned. Native Bazel JS/TS rules build both the library and tests:

```sh
bazel test //packages/browser-driver:unit_test
npm exec --yes --package=pnpm@9.15.9 -- pnpm install --frozen-lockfile --ignore-scripts
npm run build
PLAYWRIGHT_BROWSERS_PATH=/private/test-browsers node node_modules/playwright-core/cli.js install chromium
PLAYWRIGHT_BROWSERS_PATH=/private/test-browsers bazel test //packages/browser-driver:browser_test --test_env=PLAYWRIGHT_BROWSERS_PATH
```

Set the same browser path when installing and testing. Browser installation and OS libraries are explicit environment preparation; the build does not download a browser through package lifecycle scripts. The manual-tagged browser target requires matching Chromium and a working sandbox. Missing prerequisites fail the test rather than silently skipping or disabling sandboxing. The unit target participates in ordinary `bazel test //...`.

Tests exercise real pages and HTTP effects, PNG/DOM observations, structured and coordinate input, tabs, revocation, navigation/frame/handoff invalidation, bounds, lost acknowledgements and late journal admission. Their in-memory authorization/journal fixtures establish driver behavior only. Full App lifecycle, persistent GUI authority/journal adapters, encrypted profile checkpoints, cross-node fencing, ComputerView and T04/T05/T10/T24 acceptance remain incomplete.

[Source-bound component evidence](../evidence/browser-driver-2026-10-11.json) and [actual browser test output](../evidence/browser-driver-2026-10-11.log) record 11 passing cases in a disposable VM with gVisor and Chromium sandbox enabled. This is not App lifecycle or product acceptance.

[Durable App admission](54-app-admission.md) now supplies a separate reservation API. It does not yet implement the driver authority/journal callbacks.

[Browser worker bridge](55-browser-worker-bridge.md) provides the private process transport; persistent controller and node integration remain pending.
