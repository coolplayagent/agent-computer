#!/usr/bin/env python3
"""Build a minimal local component rootfs from an explicitly chosen supervisor.

Only run for trusted, locally built binaries: ldd is not safe on arbitrary input.
This is a verification fixture, not a distributable product image.
"""
import argparse
import hashlib
import json
import pathlib
import re
import shutil
import subprocess

p = argparse.ArgumentParser()
p.add_argument("--supervisor", type=pathlib.Path, required=True)
p.add_argument("--destination", type=pathlib.Path, required=True)
a = p.parse_args()
a.destination.mkdir(parents=True, exist_ok=False)
files = {}


def copy(source, destination):
    target = a.destination / destination.lstrip("/")
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, target)
    target.chmod(0o755)
    files[destination] = hashlib.sha256(target.read_bytes()).hexdigest()


for source, destination in [(a.supervisor, "/bin/agent-computer-sandbox")] + [
    (pathlib.Path(shutil.which(name)), "/bin/" + name)
    for name in ["sh", "sleep", "setsid", "env", "head", "yes", "cat", "sync", "mkdir", "mv", "rm", "rmdir"]
]:
    copy(source, destination)
    deps = subprocess.run(["ldd", str(source)], check=True, capture_output=True, text=True).stdout
    for library in re.findall(r"(/[^\s()]+)", deps):
        copy(library, library)
for name in ["proc", "dev", "tmp", "workspace"]:
    (a.destination / name).mkdir(exist_ok=True)
(a.destination / "request.json").touch()
(a.destination / "fixture-files.json").write_text(json.dumps(files, indent=2) + "\n")
print(json.dumps(files, indent=2))
