#!/usr/bin/env python3
"""Package the trusted sandbox rootfs fixture as a digest-pinned OCI archive.

Run sandbox/tests/rootfs.py first. This is test packaging, not a product image
builder or an image approval service. Import only into a disposable test cluster.
"""
import argparse
import hashlib
import json
import pathlib
import tarfile
import tempfile

p = argparse.ArgumentParser()
p.add_argument("--rootfs", type=pathlib.Path, required=True)
p.add_argument("--output", type=pathlib.Path, required=True)
a = p.parse_args()
if a.output.exists():
    raise SystemExit("output must be new")
supervisor = a.rootfs / "bin/agent-computer-sandbox"
if not supervisor.is_file() or not (a.rootfs / "fixture-files.json").is_file():
    raise SystemExit("expected trusted rootfs.py fixture")
name = "registry.invalid/agent-computer/startup-component"

with tempfile.TemporaryDirectory(prefix="ac-startup-oci-") as tmp:
    layout = pathlib.Path(tmp)
    blobs = layout / "blobs/sha256"
    blobs.mkdir(parents=True)
    layer = layout / "layer.tar"
    with tarfile.open(layer, "w") as archive:
        # Stable metadata keeps fixture bytes reproducible on this architecture.
        for path in sorted(a.rootfs.rglob("*")):
            info = archive.gettarinfo(path, arcname=str(path.relative_to(a.rootfs)))
            info.uid = info.gid = info.mtime = 0
            info.uname = info.gname = ""
            if info.isfile():
                with path.open("rb") as source:
                    archive.addfile(info, source)
            else:
                archive.addfile(info)
    layer_digest = hashlib.file_digest(layer.open("rb"), "sha256").hexdigest()
    layer_size = layer.stat().st_size
    layer.rename(blobs / layer_digest)

    def blob(value, media):
        data = json.dumps(value, separators=(",", ":")).encode()
        digest = hashlib.sha256(data).hexdigest()
        (blobs / digest).write_bytes(data)
        return {"mediaType": media, "digest": "sha256:" + digest, "size": len(data)}

    config = blob({"architecture": "amd64", "os": "linux", "config": {"User": "1000:1000"},
                   "rootfs": {"type": "layers", "diff_ids": ["sha256:" + layer_digest]}},
                  "application/vnd.oci.image.config.v1+json")
    manifest = blob({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
                     "config": config, "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar",
                                                   "digest": "sha256:" + layer_digest, "size": layer_size}]},
                    "application/vnd.oci.image.manifest.v1+json")
    image = name + "@" + manifest["digest"]
    manifest["annotations"] = {"io.containerd.image.name": image,
                               "org.opencontainers.image.ref.name": image}
    (layout / "index.json").write_text(json.dumps({"schemaVersion": 2, "manifests": [manifest]}))
    (layout / "oci-layout").write_text('{"imageLayoutVersion":"1.0.0"}')
    with tarfile.open(a.output, "w") as archive:
        for path in sorted(layout.rglob("*")):
            archive.add(path, arcname=str(path.relative_to(layout)), recursive=False)
    print(json.dumps({"image": image, "supervisor_sha256": hashlib.sha256(supervisor.read_bytes()).hexdigest(),
                      "archive_sha256": hashlib.file_digest(a.output.open("rb"), "sha256").hexdigest()}))
