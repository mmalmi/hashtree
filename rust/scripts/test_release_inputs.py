import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import release_inputs


ROOT = "rust-1.97.1-aarch64-apple-darwin"
FILENAME = ROOT + ".tar.xz"
URL = "https://static.rust-lang.org/dist/" + FILENAME
TARGET = "x86_64-unknown-linux-musl"
APK_FILENAME = "musl-1.2.5-r13.apk"
APK_URL = "https://dl-cdn.alpinelinux.org/alpine/v3.22/main/x86_64/" + APK_FILENAME


def archive_bytes(members):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w:xz") as archive:
        for name, kind, value in members:
            member = tarfile.TarInfo(name)
            if kind == "file":
                data = value.encode()
                member.size = len(data)
                member.mode = 0o755
                archive.addfile(member, io.BytesIO(data))
            else:
                member.type = tarfile.SYMTYPE if kind == "symlink" else tarfile.LNKTYPE
                member.linkname = value
                archive.addfile(member)
    return output.getvalue()


class ReleaseInputsTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="release-inputs-test-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)

    def test_cargo_uses_its_own_locked_version(self):
        host = "aarch64-apple-darwin"
        lock = {"rust_version": "1.97.1", "cargo_version": "1.97.0",
                "native": {host: {"archive": {}, "sdk": {}}}}
        environment = {"RUNNER_TEMP": str(self.root),
                       "GITHUB_ENV": str(self.root / "env"),
                       "GITHUB_PATH": str(self.root / "path")}
        with patch.dict(release_inputs.os.environ, environment), \
                patch.object(release_inputs.platform, "system", return_value="Darwin"), \
                patch.object(release_inputs.platform, "machine", return_value="arm64"), \
                patch.object(release_inputs, "configure_macos"), \
                patch.object(release_inputs, "install_component"), \
                patch.object(release_inputs, "run", side_effect=[
                    "rustc 1.97.1 (fixture)", "cargo 1.97.0 (fixture)"]), \
                contextlib.redirect_stdout(io.StringIO()):
            release_inputs.install_native(lock, host, host)
        self.assertIn("HASHTREE_PINNED_TOOLCHAIN=1", (self.root / "env").read_text())

    def test_verified_archive_reaches_installer(self):
        data = archive_bytes([(ROOT + "/install.sh", "file", "#!/bin/sh\nexit 0\n")])
        entry = {"url": URL, "sha256": hashlib.sha256(data).hexdigest()}
        with patch.object(release_inputs, "urlopen", return_value=io.BytesIO(data)), \
                patch.object(release_inputs.subprocess, "run") as execute:
            release_inputs.install_component(
                entry, FILENAME, ROOT, ["rustc", "cargo"], self.root / "prefix", self.root
            )
        execute.assert_called_once()
        arguments = execute.call_args.args[0]
        self.assertEqual(arguments[:2], ["bash", "./install.sh"])
        self.assertIn("--components=rustc,cargo", arguments)
        self.assertEqual((self.root / FILENAME).read_bytes(), data)

    def test_hash_mismatch_prevents_extraction_and_execution(self):
        data = archive_bytes([(ROOT + "/install.sh", "file", "#!/bin/sh\nexit 0\n")])
        entry = {"url": URL, "sha256": "0" * 64}
        with patch.object(release_inputs, "urlopen", return_value=io.BytesIO(data)), \
                patch.object(release_inputs, "checked_extract") as extract, \
                patch.object(release_inputs.subprocess, "run") as execute:
            with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
                release_inputs.install_component(
                    entry, FILENAME, ROOT, ["rustc"], self.root / "prefix", self.root
                )
        extract.assert_not_called()
        execute.assert_not_called()

    def extract(self, members):
        archive = self.root / "fixture.tar.xz"
        archive.write_bytes(archive_bytes(members))
        return release_inputs.checked_extract(archive, self.root / "extracted", ROOT)

    def test_archive_parent_traversal_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "Unsafe archive member"):
            self.extract([(ROOT + "/../escape", "file", "bad")])
        self.assertFalse((self.root / "escape").exists())

    def test_absolute_archive_path_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "Unsafe archive member"):
            self.extract([("/escape", "file", "bad")])

    def test_archive_symlink_escape_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "escapes its release directory"):
            self.extract([(ROOT + "/bin/link", "symlink", "../../../escape")])

    def test_archive_hardlink_escape_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "escapes its release directory"):
            self.extract([(ROOT + "/link", "hardlink", "../escape")])

    def test_internal_relative_symlink_is_allowed(self):
        directory = self.extract([
            (ROOT + "/install.sh", "file", "#!/bin/sh\nexit 0\n"),
            (ROOT + "/lib/tool", "file", "valid"),
            (ROOT + "/bin/tool", "symlink", "../lib/tool"),
        ])
        self.assertEqual((directory / "bin/tool").read_text(), "valid")

    def linux_command(self, packages, command="linux-packages"):
        manifest = self.root / "release-inputs.json"
        manifest.write_text(json.dumps({
            "linux": {TARGET: {"image": "rust:1.97-alpine3.22@sha256:" + "1" * 64,
                               "packages": packages}}
        }))
        output = io.StringIO()
        with patch("sys.argv", ["release_inputs.py", "--manifest", str(manifest),
                                command, "--target", TARGET]), contextlib.redirect_stdout(output):
            release_inputs.main()
        return output.getvalue()

    def test_linux_package_output_is_exact(self):
        digest = "a" * 64
        output = self.linux_command([{"filename": APK_FILENAME, "url": APK_URL, "sha256": digest}])
        self.assertEqual(output, f"{digest} {APK_FILENAME} {APK_URL}\n")

    def test_linux_image_output_is_digest_pinned(self):
        self.assertEqual(self.linux_command([], "linux-image"),
                         "rust:1.97-alpine3.22@sha256:" + "1" * 64 + "\n")

    def test_malformed_apk_hash_is_rejected(self):
        for digest in ("a" * 63, "G" * 64, "a" * 64 + "\n", "a" * 64 + " extra"):
            with self.subTest(digest=digest), self.assertRaisesRegex(ValueError, "Invalid APK SHA-256"):
                self.linux_command([{"filename": APK_FILENAME, "url": APK_URL, "sha256": digest}])

    def test_untrusted_apk_urls_are_rejected(self):
        urls = (
            APK_URL.replace("https://", "http://"),
            APK_URL.replace("dl-cdn.alpinelinux.org", "untrusted.example"),
            APK_URL.replace("https://", "https://user:password@"),
            APK_URL + "?mutable=1",
            APK_URL + "#fragment",
            APK_URL.replace(APK_FILENAME, "other.apk"),
            APK_URL.replace("/alpine/", "/al pine/"),
        )
        for url in urls:
            with self.subTest(url=url), self.assertRaises(ValueError):
                self.linux_command([{"filename": APK_FILENAME, "url": url, "sha256": "a" * 64}])

    def test_apk_path_traversal_and_duplicate_names_are_rejected(self):
        valid = {"filename": APK_FILENAME, "url": APK_URL, "sha256": "a" * 64}
        for packages in ([dict(valid, filename="../" + APK_FILENAME)], [valid, valid]):
            with self.subTest(packages=packages), self.assertRaisesRegex(ValueError, "Invalid or duplicate"):
                self.linux_command(packages)

    def test_rust_archive_origin_and_filename_are_checked_before_network(self):
        for url in (URL.replace("static.rust-lang.org", "untrusted.example"), URL + "?x=1",
                    URL.replace(FILENAME, "rust-other.tar.xz")):
            with self.subTest(url=url), patch.object(release_inputs, "urlopen") as download:
                with self.assertRaises(ValueError):
                    release_inputs.checked_download({"url": url, "sha256": "a" * 64},
                                                    self.root / "download", FILENAME)
                download.assert_not_called()


class WindowsEnvironmentProfileTests(unittest.TestCase):
    def setUp(self):
        self.old_profile = {
            "image_version": "20261026.347.1",
            "visual_studio_version": "17.14.37824.6",
            "msvc_tools_version": "14.44.35207",
            "windows_sdk_version": "10.0.26100.0",
        }
        self.new_profile = dict(self.old_profile, image_version="20261101.353.1",
                                visual_studio_version="17.14.37827.9")
        self.sdk = {"profiles": [self.old_profile, self.new_profile]}

    def configure(self, expected, sdk=None, visual_studio=None, msvc=None, windows_sdk=None):
        environment = {"ImageVersion": expected["image_version"],
                       "ProgramFiles(x86)": "/fixture/program-files", "PATH": "/fixture/original"}
        installations = json.dumps([{
            "installationPath": "/fixture/visual-studio",
            "installationVersion": visual_studio or expected["visual_studio_version"],
        }])
        compiler = "\n".join([
            "VCToolsVersion=" + (msvc or expected["msvc_tools_version"]),
            "WindowsSDKVersion=" + (windows_sdk or expected["windows_sdk_version"]) + "\\",
            "Path=/fixture/compiler" + os.pathsep + "/fixture/original",
            "INCLUDE=/fixture/include",
        ])
        with patch.dict(release_inputs.os.environ, environment), \
                patch.object(release_inputs.Path, "is_dir", return_value=True), \
                patch.object(release_inputs, "run", side_effect=[installations, compiler]) as execute, \
                patch.object(release_inputs, "export_environment") as export, \
                patch.object(release_inputs, "append_actions_file"):
            release_inputs.configure_windows(self.sdk if sdk is None else sdk)
        return execute, export

    def test_each_locked_image_selects_its_exact_visual_studio(self):
        for profile in self.sdk["profiles"]:
            with self.subTest(image=profile["image_version"]):
                execute, export = self.configure(profile)
                command = execute.call_args.args[0][-1]
                self.assertIn("-vcvars_ver=" + profile["msvc_tools_version"], command)
                self.assertIn(profile["windows_sdk_version"], command)
                self.assertEqual(export.call_args.args[0]["VCTOOLSVERSION"], profile["msvc_tools_version"])

    def test_previous_single_profile_format_still_works(self):
        execute, _ = self.configure(self.old_profile, sdk=self.old_profile)
        self.assertEqual(execute.call_count, 2)

    def test_unknown_image_stops_before_compiler_discovery(self):
        with patch.dict(release_inputs.os.environ, {"ImageVersion": "20261109.400.1"}), \
                patch.object(release_inputs, "run") as execute:
            with self.assertRaisesRegex(ValueError, "no unique locked environment"):
                release_inputs.configure_windows(self.sdk)
        execute.assert_not_called()

    def test_duplicate_matching_profiles_are_rejected(self):
        sdk = {"profiles": [self.old_profile, dict(self.old_profile)]}
        with patch.dict(release_inputs.os.environ, {"ImageVersion": self.old_profile["image_version"]}), \
                patch.object(release_inputs, "run") as execute:
            with self.assertRaisesRegex(ValueError, "no unique locked environment"):
                release_inputs.configure_windows(sdk)
        execute.assert_not_called()

    def test_wildcard_image_profile_is_rejected(self):
        sdk = {"profiles": [dict(self.old_profile, image_version="202611*.1")]}
        with patch.object(release_inputs, "run") as execute:
            with self.assertRaisesRegex(ValueError, "Invalid Windows runner image profile"):
                release_inputs.configure_windows(sdk)
        execute.assert_not_called()

    def test_different_visual_studio_is_rejected_for_locked_image(self):
        with self.assertRaisesRegex(ValueError, "exact Visual Studio installation"):
            self.configure(self.new_profile, visual_studio=self.old_profile["visual_studio_version"])

    def test_different_compiler_or_sdk_still_fails_after_profile_selection(self):
        for changed in ({"msvc": "14.44.35208"}, {"windows_sdk": "10.0.26100.1"}):
            with self.subTest(changed=changed), self.assertRaisesRegex(ValueError, "selected a different"):
                self.configure(self.new_profile, **changed)


if __name__ == "__main__":
    unittest.main()
