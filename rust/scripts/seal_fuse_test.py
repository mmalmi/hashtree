#!/usr/bin/env python3
"""Seal a hosted FUSE test executable without running it."""
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

root = Path(__file__).resolve().parents[2]
def output(*args):
    return subprocess.check_output(args, cwd=root, text=True).strip()

executables = set()
for line in (root / "rust/fuse-build.jsonl").read_text().splitlines():
    event = json.loads(line)
    if (event.get("reason") == "compiler-artifact"
            and event.get("target", {}).get("name") == "hashtree_fuse"
            and event.get("profile", {}).get("test")
            and event.get("executable")):
        executables.add(Path(event["executable"]))
if len(executables) != 1:
    raise SystemExit("Expected exactly one FUSE library test executable")
binary = executables.pop()
size = binary.stat().st_size
if size > 64 * 1024 * 1024:
    raise SystemExit("FUSE qualification executable exceeds 64 MiB admission limit")
dynamic = output("readelf", "-d", str(binary))
if "libfuse" in dynamic:
    raise SystemExit("Pure Linux backend still links libfuse")
if output("git", "status", "--porcelain", "--untracked-files=no"):
    raise SystemExit("Tracked source changed during build")
destination = root / "fuse-native"
destination.mkdir(exist_ok=False)
shutil.copyfile(binary, destination / "fuse-native-test")
(destination / "fuse-native-test").chmod(0o755)
receipt = {
    "schema": 1,
    "commit": output("git", "rev-parse", "HEAD"),
    "tree": output("git", "rev-parse", "HEAD^{tree}"),
    "rustc": output("rustc", "-vV"),
    "lock_sha256": hashlib.sha256((root / "rust/Cargo.lock").read_bytes()).hexdigest(),
    "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
    "binary_bytes": size,
    "dynamic": dynamic,
    "version_requirements": output("readelf", "--version-info", str(binary)),
    "runtime_executed": False,
}
(destination / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
print(json.dumps({"binary_bytes": size, "binary_sha256": receipt["binary_sha256"]}))
