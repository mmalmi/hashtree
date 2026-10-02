"""Exercise the link gate's manifest bindings without running Rust tools."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
PACKAGES = {
    "hashtree-core": ("crates/hashtree-core", "0.2.91"),
    "git-remote-htree": ("crates/git-remote-htree", "0.2.90"),
    "hashtree-blossom": ("crates/hashtree-blossom", "0.2.83"),
    "hashtree-lmdb": ("crates/hashtree-lmdb", "0.2.89"),
    "hashtree-lmdb-master-sys": ("vendor/lmdb-master-sys", "0.2.6-hashtree.2"),
    "hashtree-heed": ("vendor/heed", "0.20.5-hashtree.2"),
    "hashtree-nostr-social-graph-heed": ("vendor/nostr-social-graph-heed", "0.1.3-hashtree.3"),
}
FAKE_CARGO = r'''
import io, json, os, pathlib, re, sys, tarfile
args = sys.argv[1:]
versions = json.loads(os.environ["TEST_PACKAGE_VERSIONS"])
with open(os.environ["TEST_CARGO_CALLS"], "a") as log:
    log.write(json.dumps(args) + "\n")
if args[0] == "tree":
    if "--manifest-path" in args:
        manifest = pathlib.Path(args[args.index("--manifest-path") + 1]).read_text()
        paths = sorted(set(re.findall(r'path = "([^"]+)"', manifest)))
        for path in paths:
            package = pathlib.Path(path)
            assert (package / "Cargo.toml").is_file(), path
            name = next(name for name in versions if package.name == name + "-" + versions[name])
            print(name, "v" + versions[name], "(" + path + ")")
    else:
        for name, version in versions.items():
            print(name, "v" + version)
        if os.environ.get("TEST_EXTRA_LMDB"):
            print("lmdb-master-sys v0.2.6")
elif args[0] == "package":
    output = pathlib.Path(os.environ["CARGO_TARGET_DIR"]) / "package"
    output.mkdir(parents=True)
    for name, version in versions.items():
        with tarfile.open(output / (name + "-" + version + ".crate"), "w:gz") as archive:
            data = ('[package]\nname = "' + name + '"\nversion = "' + version + '"\n').encode()
            entry = tarfile.TarInfo(name + "-" + version + "/Cargo.toml")
            entry.size = len(data)
            archive.addfile(entry, io.BytesIO(data))
elif args[0] == "build":
    target = pathlib.Path(args[args.index("--target-dir") + 1]) / "debug"
    target.mkdir(parents=True)
    binary = target / "hashtree-unified-lmdb-downstream"
    binary.write_text("#!/bin/sh\nexit 0\n")
    binary.chmod(0o755)
else:
    raise AssertionError(args)
'''


class UnifiedLmdbManifestBindings(unittest.TestCase):
    def run_gate(self, versions=None, extra_lmdb=False, missing_version=None):
        versions = versions or {name: version for name, (_, version) in PACKAGES.items()}
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp).resolve()
            rust = root / "rust"
            (rust / "tests").mkdir(parents=True)
            script = rust / "tests/test_unified_lmdb_link.sh"
            shutil.copy2(ROOT / "tests/test_unified_lmdb_link.sh", script)
            for name, (relative, _) in PACKAGES.items():
                manifest = rust / relative / "Cargo.toml"
                manifest.parent.mkdir(parents=True)
                content = '[package]\nname = "' + name + '"\n'
                if name != missing_version:
                    content += 'version = "' + versions[name] + '"\n'
                manifest.write_text(content)
            tools = root / "tools"
            tools.mkdir()
            cargo = tools / "cargo"
            cargo.write_text("#!" + sys.executable + "\n" + FAKE_CARGO)
            cargo.chmod(0o755)
            calls = root / "calls.jsonl"
            env = {**os.environ, "PATH": str(tools) + os.pathsep + os.environ["PATH"],
                   "CARGO_TARGET_DIR": str(root / "target"), "TMPDIR": str(root),
                   "TEST_PACKAGE_VERSIONS": json.dumps(versions), "TEST_CARGO_CALLS": str(calls),
                   "TEST_EXTRA_LMDB": "1" if extra_lmdb else ""}
            result = subprocess.run(["bash", str(script)], env=env, text=True,
                                    capture_output=True, timeout=10)
            commands = [json.loads(line) for line in calls.read_text().splitlines()] if calls.exists() else []
            return result, commands

    def test_archive_paths_and_both_dependency_trees_follow_manifests(self):
        current = {name: version for name, (_, version) in PACKAGES.items()}
        future = {**current, "hashtree-lmdb-master-sys": "0.2.6-hashtree.7",
                  "hashtree-heed": "0.20.5-hashtree.8",
                  "hashtree-nostr-social-graph-heed": "0.1.3-hashtree.9"}
        for versions in [current, future]:
            with self.subTest(versions=versions):
                result, calls = self.run_gate(versions)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual([call[0] for call in calls], ["tree", "package", "tree", "build"])
                self.assertIn("--locked", calls[-1])
                self.assertIn("link checks passed", result.stdout)

    def test_duplicate_native_lmdb_still_stops_before_packaging(self):
        result, calls = self.run_gate(extra_lmdb=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([call[0] for call in calls], ["tree"])
        self.assertIn("exactly one hardened native LMDB package", result.stderr)

    def test_missing_vendor_version_stops_before_tool_invocation(self):
        result, calls = self.run_gate(missing_version="hashtree-heed")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(calls, [])
        self.assertIn("missing hashtree-heed package version", result.stderr)


if __name__ == "__main__":
    unittest.main()
