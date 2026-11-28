"""Exercise the actual publisher loop with fake Cargo and a read-only registry."""
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch
from urllib.error import HTTPError
from urllib.parse import urlparse


script = Path(__file__).resolve().parents[1] / "scripts/publish_crates.py"
spec = importlib.util.spec_from_file_location("publish_crates", script)
publisher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(publisher)


def crate(name, revision="a" * 40, changed=None, mode=0o644):
    files = {
        ".cargo_vcs_info.json": json.dumps({"git": {"sha1": revision}, "path_in_vcs": name}).encode(),
        "Cargo.toml": f'[package]\nname="{name}"\nversion="1.0.0"'.encode(),
        "Cargo.toml.orig": b"[package]\nversion.workspace=true",
        "Cargo.lock": b"one reviewed lock",
        "src/lib.rs": b"pub fn build() {}",
    }
    files.update(changed or {})
    result = io.BytesIO()
    with tarfile.open(fileobj=result, mode="w:gz") as archive:
        for path, data in files.items():
            info = tarfile.TarInfo(f"{name}-1.0.0/{path}")
            info.size = len(data)
            info.mode = mode if path == "src/lib.rs" else 0o644
            archive.addfile(info, io.BytesIO(data))
    return result.getvalue()


class PublishResumeTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.target = Path(temp.name)
        self.plan = ["leaf", "hashtree-cli"]
        self.local = {name: crate(name) for name in self.plan}
        self.published = {}
        self.races = {}
        self.yanked = set()
        self.uploads = []
        self.invalid_download = False

    def read_registry(self, request, **kwargs):
        url = request if isinstance(request, str) else request.full_url
        address = urlparse(url)
        if address.hostname == "index.crates.io":
            name = address.path.rsplit("/", 1)[-1]
            if name not in self.published:
                raise HTTPError(url, 404, "unknown crate", {}, None)
            entry = {"name": name, "vers": "1.0.0", "yanked": name in self.yanked,
                     "cksum": hashlib.sha256(self.published[name]).hexdigest()}
            return io.BytesIO(json.dumps(entry).encode() + b"\n")
        self.assertEqual(address.hostname, "static.crates.io")
        name = address.path.split("/")[2]
        return io.BytesIO(b"incomplete download" if self.invalid_download else self.published[name])

    def cargo(self, argv, **kwargs):
        if argv[1] == "package":
            self.assertIn("--locked", argv)
            self.assertIn("--no-verify", argv)
            directory = Path(argv[argv.index("--target-dir") + 1]) / "package"
            directory.mkdir(parents=True, exist_ok=True)
            for name in self.plan:
                (directory / f"{name}-1.0.0.crate").write_bytes(self.local[name])
            return subprocess.CompletedProcess(argv, 0, "")
        self.assertEqual(argv[:2], ["cargo", "publish"])
        name = argv[argv.index("-p") + 1]
        self.uploads.append(name)
        if name in self.races:
            self.published[name] = self.races[name]
        if name in self.published:
            return subprocess.CompletedProcess(argv, 1, f"error: crate {name}@1.0.0 already exists on crates.io index\n")
        self.published[name] = self.local[name]
        return subprocess.CompletedProcess(argv, 0, "published\n")

    def run_publisher(self):
        packages = [
            {"name": "leaf", "version": "1.0.0", "publish": None, "dependencies": []},
            {"name": "hashtree-cli", "version": "1.0.0", "publish": None,
             "dependencies": [{"name": "leaf", "path": "leaf", "req": "=1.0.0", "kind": None}]},
        ]
        meta = json.dumps({"packages": packages, "target_directory": str(self.target)})
        with patch.object(sys, "argv", [str(script)]), patch("subprocess.check_output", return_value=meta), \
             patch("subprocess.run", side_effect=self.cargo), patch("urllib.request.urlopen", side_effect=self.read_registry):
            publisher.main()

    def test_identical_and_unchanged_carried_forward_do_not_upload(self):
        self.published["leaf"] = self.local["leaf"]
        self.published["hashtree-cli"] = crate("hashtree-cli", revision="b" * 40)
        self.run_publisher()
        self.assertEqual([], self.uploads)

    def test_late_conflict_blocks_every_upload(self):
        for change in ["src/lib.rs", "Cargo.toml", "Cargo.toml.orig", "Cargo.lock", "new-file"]:
            with self.subTest(change=change):
                self.published = {"hashtree-cli": crate("hashtree-cli", changed={change: b"different"})}
                with self.assertRaises(SystemExit):
                    self.run_publisher()
                self.assertEqual([], self.uploads)
        self.published = {"hashtree-cli": crate("hashtree-cli", mode=0o755)}
        with self.assertRaises(SystemExit):
            self.run_publisher()
        self.assertEqual([], self.uploads)

    def test_unverifiable_or_yanked_version_blocks_every_upload(self):
        self.published = {"hashtree-cli": crate("hashtree-cli", revision="b" * 40)}
        self.invalid_download = True
        with self.assertRaises(SystemExit):
            self.run_publisher()
        self.invalid_download = False
        self.yanked.add("hashtree-cli")
        with self.assertRaises(SystemExit):
            self.run_publisher()
        self.assertEqual([], self.uploads)

    def test_racing_duplicate_rechecks_identity_before_dependents(self):
        self.races["leaf"] = crate("leaf", changed={"src/lib.rs": b"different"})
        with self.assertRaises(SystemExit):
            self.run_publisher()
        self.assertEqual(["leaf"], self.uploads)
        self.published.clear()
        self.uploads.clear()
        self.races["leaf"] = self.local["leaf"]
        self.run_publisher()
        self.assertEqual(self.plan, self.uploads)


if __name__ == "__main__":
    unittest.main()
