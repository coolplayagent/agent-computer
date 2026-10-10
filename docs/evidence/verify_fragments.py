#!/usr/bin/env python3
"""Verify and optionally reconstruct a digest-bound JSON evidence record."""

import argparse
import hashlib
import json
from pathlib import Path


def reconstruct(manifest: Path):
    data = manifest.read_bytes()
    if len(data) > 1024 * 1024:
        raise ValueError("manifest exceeds 1 MiB")
    record = json.loads(data)
    details = record.pop("detail_artifacts")
    entries = details["entries"]
    if details["schema_version"] != 2 or not 1 <= len(entries) <= 512:
        raise ValueError("unsupported fragment contract")
    root = manifest.resolve().parent
    for entry in entries:
        path = (root / entry["path"]).resolve()
        if not path.is_relative_to(root) or path == root:
            raise ValueError("fragment escapes evidence directory")
        data = path.read_bytes()
        if len(data) > 8192 or hashlib.sha256(data).hexdigest() != entry["sha256"]:
            raise ValueError(f"fragment size or digest mismatch: {entry['path']}")
        value = json.loads(data)
        target = record
        for token in entry["target"]:
            if isinstance(target, dict) and isinstance(token, str):
                target = target[token]
            elif isinstance(target, list) and type(token) is int and token >= 0:
                target = target[token]
            else:
                raise ValueError("invalid target token")
        if entry["operation"] == "merge":
            if not isinstance(target, dict) or not isinstance(value, dict):
                raise ValueError("merge requires two dictionaries")
            if target.keys() & value.keys():
                raise ValueError("fragment overwrites an existing key")
            target.update(value)
        elif entry["operation"] == "append":
            if not isinstance(target, list) or not isinstance(value, list):
                raise ValueError("append requires two arrays")
            if entry["start"] != len(target):
                raise ValueError("fragment array offset is not contiguous")
            target.extend(value)
        else:
            raise ValueError("unknown fragment operation")
    canonical = json.dumps(
        record, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")
    digest = hashlib.sha256(canonical).hexdigest()
    if digest != details["original_canonical_sha256"]:
        raise ValueError("reconstructed record digest mismatch")
    return record, digest, len(entries)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    record, digest, count = reconstruct(args.manifest)
    if args.output:
        args.output.write_text(json.dumps(record, indent=2, ensure_ascii=False) + "\n")
    print(f"verified {count} fragments; canonical SHA-256 {digest}")


if __name__ == "__main__":
    main()
