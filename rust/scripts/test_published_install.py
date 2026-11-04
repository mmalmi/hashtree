#!/usr/bin/env python3
"""Regression checks for canonical and GitHub published asset layouts."""

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


if __name__ == "__main__":
    unittest.main()
