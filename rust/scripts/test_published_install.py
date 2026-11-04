#!/usr/bin/env python3
"""Regression checks for canonical and GitHub published asset layouts."""

import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import textwrap
import unittest
from verify_published_install import published_asset_url


class PublishedAssetUrls(unittest.TestCase):
    def test_canonical_directory_keeps_assets_prefix(self):
        self.assertEqual(
            published_asset_url("https://example.test/v0.3.1/", "", "cli.tar.gz", "assets/cli.tar.gz"),
            "https://example.test/v0.3.1/assets/cli.tar.gz",
        )

    def test_github_archive_uses_explicit_flat_directory(self):
        base = "https://github.com/mmalmi/hashtree/releases/download/v0.3.1"
        self.assertEqual(published_asset_url(base, base, "cli.tar.gz", "assets/cli.tar.gz"),
                         base + "/cli.tar.gz")

    def test_manifest_stays_at_the_release_root(self):
        self.assertEqual(
            published_asset_url("https://example.test/release", "https://example.test/assets",
                                "release.json", "release.json"),
            "https://example.test/release/release.json",
        )

    def test_release_workflow_writes_parseable_component_manifest(self):
        workflow = (Path(__file__).resolve().parents[2] / ".github/workflows/release.yml").read_text()
        stage = workflow.split("      - name: Bind the staged assets to source and reviewed inputs", 1)[1]
        body = re.search(r"          python3 - <<'PY'\n(.*?)\n          PY", stage, re.S).group(1)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            versions = {"hashtree-cli": "0.3.1", "git-remote-htree": "0.3.0",
                        "hashtree-core": "0.3.0", "hashtree-cashu-cli": "0.3.0"}
            for name, version in versions.items():
                package = root / "rust/crates" / name
                package.mkdir(parents=True)
                (package / "Cargo.toml").write_text(
                    f'[package]\nname = "{name}"\nversion = "{version}"\n')
            (root / "staged-release").mkdir()
            (root / "artifacts/hashtree").mkdir(parents=True)
            (root / "staged-release/release.json").write_text(json.dumps(
                {"tag": "v0.3.1", "commit": "a" * 40, "assets": []}))
            subprocess.run([sys.executable, "-"], input=textwrap.dedent(body),
                           cwd=root, text=True, check=True, capture_output=True)
            result = json.loads((root / "artifacts/hashtree/release.json").read_text())
            self.assertEqual(result["components"], versions)
            self.assertEqual(result["tag"], "v0.3.1")


if __name__ == "__main__":
    unittest.main()
