# 07. ComputerSet declaration validation

## 07.1 Usage

Run from the repository root:

```bash
bazel run //:agent-computer -- validate examples/research.computer.yaml --json
bazel run //:agent-computer -- schema computer-set --json
```

The CLI accepts a file or `-` (stdin), with optional `--format yaml|json`. Without an override, `.json` files use JSON; other inputs use YAML. YAML accepts a JSON-compatible data subset; custom tags, multiple documents, and duplicate keys are rejected. Example image digests are placeholders. Static validation never pulls images, creates resources, or reads credentials.

Exit codes: 0 means statically valid; 1 means invalid declaration; 2 means usage or input read failure. `--json` writes one report to stdout. Plain-text errors go to stderr. Errors do not echo source content, unknown field values, or secrets. Syntax diagnostics include available line/column positions, structure diagnostics point to the exported schema, and semantic diagnostics use JSON Pointers.

## 07.2 Structure and semantics

The [JSON Schema](../../schemas/computer-set-v1alpha1.json) is generated from Rust DTOs and covers types, required fields, enums, and unknown-field rejection. The crate's validator enforces additional semantics. Run `validate` even after a document passes schema validation.

1. Use fixed `apiVersion: agent-computer/v1alpha1` and `kind: ComputerSet`. Declarations cannot supply status, Pod UID, organization/principal identity, raw env, or secrets fields.
2. Volume, Workspace, Sandbox, App, Agent, and Computer can be declared independently. Names are unique within each kind and contain 1–63 lowercase ASCII letters, digits, or hyphens, with alphanumeric ends. Update precondition `expectedRevision` must be a positive signed 64-bit integer; omit it for creation.
3. Local references name a resource of the expected kind in the document. Existing resources use `id:<opaque-id>` with 1–128 ASCII letters, digits, underscores, or hyphens. Missing local references fail. Existing IDs, storageClass, networkPolicyRef, profileRef, and secretRefs appear in `external_references` for later checks.
4. References follow Computer → App → Sandbox → Workspace → Volume; Agents may reference Sandboxes. This schema cannot express cycles pointing back to a higher layer. Computers must include each local App's Sandbox. Writable mounts use the primary Workspace.
5. Sandbox declarations currently recognize `gvisor`. Image references are parsed by `oci-spec` and require a fixed lowercase SHA-256 digest; floating tags are rejected. CPU, memory, and Volume quota must be positive. Volumes require Retain; Workspaces require explicit conflict handling.
6. `mounts` accept workspaceRef/path/readOnly only. Destinations must be normalized below `/workspace`, `/inputs`, `/app`, or `/data` and must not overlap. Actual Candidates, independent writable inodes, and network isolation require future adapters.
7. Browser uses `chromium-playwright` with an explicit profileRef. The Driver owns launch parameters, health, and profile paths, so a declaration cannot override them with `--no-sandbox`. `web-application` requires argv, cwd, and health and cannot use profileRef. statePaths/exportPaths provide explicit path allowlists. Health requires a nonzero port, normalized absolute HTTP path, and a 1–3600 second startup budget.
8. Agents are optional. `external` has no sandboxRef; `hosted` requires one. The current schema recognizes the `tools-api` adapter contract. Capabilities express requirements and confer no permissions. secretRefs accept references; plaintext secrets fields are rejected.

## 07.3 Digests and budgets

Successful reports contain `scope: static`, `definition_digest`, resource count, and references requiring verification. `valid: true` establishes local structure and semantics only. Services must still check reference type, organization, ACL, revision, image/driver compatibility, and deployment compatibility. `plan/apply` and runtime capabilities remain unsupported.

Canonicalization is versioned as `agent-computer/definition-v1`: fill DTO defaults; order resources by name, mounts by path, references and path sets lexically, and capabilities by the schema's enum order. Preserve argv order. Serialize sorted object keys as compact UTF-8 JSON, then compute `SHA256(version string + NUL + JSON)`. The digest includes names and expectedRevision and identifies the complete declaration intent, not an individual runtime spec. YAML comments, whitespace, object-key order, and unordered collection permutations do not change it.

Input and cumulative expanded key/string bytes are each limited to 1 MiB. Budgets allow 65,536 value nodes, 32 nesting levels, 1,024 resources, and 128 mounts per Sandbox. Reports include at most 100 diagnostics with an explicit truncation flag. Duplicate keys fail rather than overwriting earlier values. YAML aliases consume expansion budgets too.

## 07.4 Verification record

Coverage includes independent human declarations, unknown fields/capabilities, reference mismatches, cross-organization syntax, resource/mount constraints, driver field conflicts, duplicate keys, alias expansion, digest stability, schema synchronization, and CLI exit codes. An independent Python Draft 2020-12 validator checks example acceptance and unknown-field rejection; an independent SHA-256 calculation matches the Rust example digest.

Parsing uses [serde_yaml_ng](https://docs.rs/serde_yaml_ng/0.10.0/serde_yaml_ng/), and image syntax uses [oci-spec Reference](https://docs.rs/oci-spec/0.10.0/oci_spec/distribution/struct.Reference.html). Cargo.lock pins crate versions; Bazel crate_universe generates dependencies from the same workspace and lockfile.
