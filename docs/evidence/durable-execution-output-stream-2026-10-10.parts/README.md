# Durable output evidence fragments

These 167 native JSON fragments retain the original execution, object-store,
node, Candidate and source-binding evidence. Each fragment is at most 8 KiB.
The [main record](../durable-execution-output-stream-2026-10-10.json) binds every
fragment by SHA-256 and records its target and ordered reconstruction operation.
Empty containers in that record are reconstruction placeholders, not evidence
of zero rows or missing observations. No test result or source record was dropped.

The earlier 223 KiB cases file expanded to 24,864,377 bytes of owned graph facts,
exceeding the immutable 16 MiB writer quantum. File size alone therefore did not
prove indexability. These fragments retain structured JSON and are validated
through an isolated incremental index using the normal frozen limits.

From the repository root, verify every fragment and the reconstructed record:

```bash
python3 docs/evidence/verify_fragments.py \
  docs/evidence/durable-execution-output-stream-2026-10-10.json
```

Add `--output /tmp/durable-output-evidence.json` to materialize the original
record. Canonical JSON uses sorted object keys, compact separators and UTF-8;
its original SHA-256 remains
`3b364cb29c1c82ab153502c9721b33ec6260cca60030e7df307ce3c088676bbd`.
