#!/usr/bin/env python3
"""Payload fixtures only: never compile, download or publish anything."""
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tarfile
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch
import zipfile


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("build_haps_payload", Path(__file__).with_name("build_haps_payload.py"))
recipe = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(recipe)


class PayloadTests(unittest.TestCase):
    def test_native_setup_selects_the_actual_host_and_requested_target(self):
        cases = [
            ("Darwin", "arm64", "aarch64-apple-darwin", "x86_64-apple-darwin"),
            ("Darwin", "aarch64", "aarch64-apple-darwin", "aarch64-apple-darwin"),
            ("Darwin", "x86_64", "x86_64-apple-darwin", "aarch64-apple-darwin"),
            ("Windows", "AMD64", "x86_64-pc-windows-msvc", "x86_64-pc-windows-msvc"),
        ]
        for system, machine, host, target in cases:
            with self.subTest(system=system, machine=machine, target=target), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                (root / "rust").mkdir()
                lock = {"rust_version": "1.97.1"}
                (root / "rust/release-inputs.json").write_text(json.dumps(lock))
                inputs = SimpleNamespace(install_native=Mock())
                specification = SimpleNamespace(loader=SimpleNamespace(exec_module=Mock()))
                with patch.object(recipe.platform, "system", return_value=system), \
                     patch.object(recipe.platform, "machine", return_value=machine), \
                     patch.object(recipe.importlib.util, "spec_from_file_location", return_value=specification), \
                     patch.object(recipe.importlib.util, "module_from_spec", return_value=inputs), \
                     patch.dict(os.environ):
                    recipe.prepare_native(root, root, target)
                    inputs.install_native.assert_called_once_with(lock, host, target)
                    self.assertEqual(os.environ["PATH"].split(os.pathsep)[0],
                                     str(root / "native-inputs/pinned-rust/bin"))
                    self.assertEqual(os.environ["GITHUB_ENV"], str(root / "native-inputs/environment"))

    def test_all_existing_platforms_retain_their_release_abi(self):
        self.assertEqual(recipe.TARGETS, {
            "x86_64-unknown-linux-gnu": "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-gnu": "aarch64-unknown-linux-musl",
            "x86_64-apple-darwin": "x86_64-apple-darwin",
            "aarch64-apple-darwin": "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc": "x86_64-pc-windows-msvc",
        })

    @unittest.skipIf(os.name == "nt", "POSIX archive and executable metadata fixture")
    def test_unix_official_packager_retains_binaries_readme_installer_and_modes(self):
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary)
            target = "x86_64-unknown-linux-musl"
            binaries = work / "cargo" / target / "release"
            binaries.mkdir(parents=True)
            for name in ("htree", "git-remote-htree", "htree-cashu"):
                binary = binaries / name
                binary.write_bytes(("fixture " + name).encode())
                binary.chmod(0o755)
            subprocess.run([
                "bash", str(ROOT / "rust/scripts/build_release_artifacts.sh"),
                "--version", "v0.5.0", "--targets", target, "--package-only",
                "--repo-dir", str(ROOT), "--target-dir", str(work / "cargo"),
                "--output-dir", str(work / "archives"), "--linux-builder", "docker",
            ], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            archive = work / "archives" / ("hashtree-" + target + ".tar.gz")
            recipe.stage_archive(archive, work / "payload")
            with tarfile.open(archive) as bundle:
                members = [member for member in bundle if member.isfile()]
                self.assertEqual({member.name for member in members}, {
                    "hashtree/htree", "hashtree/git-remote-htree", "hashtree/htree-cashu",
                    "hashtree/README.txt", "hashtree/install.sh",
                })
                for member in members:
                    output = work / "payload" / Path(member.name).relative_to("hashtree")
                    self.assertEqual(output.read_bytes(), bundle.extractfile(member).read())
                    self.assertEqual(stat.S_IMODE(output.stat().st_mode),
                                     0o755 if member.mode & 0o111 else 0o644)

    def test_zip_preserves_bytes_and_haps_executable_mapping(self):
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary)
            archive = work / "release.zip"
            with zipfile.ZipFile(archive, "w") as bundle:
                for name in ("htree.exe", "git-remote-htree.exe", "htree-cashu.exe", "README.txt"):
                    info = zipfile.ZipInfo("hashtree/" + name)
                    info.create_system = 0
                    info.external_attr = 0x20
                    bundle.writestr(info, b"DOS archive fixture\r\n")
            recipe.stage_archive(archive, work / "payload")
            for output in (work / "payload").iterdir():
                self.assertEqual(output.read_bytes(), b"DOS archive fixture\r\n")
                if os.name != "nt":
                    self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o644)

    def test_rejects_escaped_or_linked_archive_files(self):
        for name, kind in (("hashtree/../../escape", tarfile.REGTYPE),
                           ("other/file", tarfile.REGTYPE),
                           ("hashtree/link", tarfile.SYMTYPE)):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                work = Path(temporary)
                with tarfile.open(work / "bad.tar", "w") as bundle:
                    entry = tarfile.TarInfo(name)
                    entry.type = kind
                    entry.size = 1 if kind == tarfile.REGTYPE else 0
                    entry.linkname = "../../escape" if kind == tarfile.SYMTYPE else ""
                    bundle.addfile(entry, io.BytesIO(b"x") if entry.size else None)
                with self.assertRaises(ValueError):
                    recipe.stage_archive(work / "bad.tar", work / "payload")
                self.assertFalse((work / "escape").exists())

    def test_linux_recipe_explicitly_uses_the_pinned_docker_builder(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = root / "rust/crates/hashtree-cli/Cargo.toml"
            manifest.parent.mkdir(parents=True)
            manifest.write_text('version = "0.5.0"\n')
            image = "rust:fixture@sha256:" + "a" * 64
            (root / "rust/release-inputs.json").write_text(json.dumps({
                "linux": {"aarch64-unknown-linux-musl": {"image": image}}}))
            with patch.object(recipe.platform, "system", return_value="Linux"), \
                 patch.object(recipe.subprocess, "run") as run, \
                 patch.object(recipe, "stage_archive") as stage, \
                 patch.object(recipe, "prepare_native") as native:
                recipe.build(root, "aarch64-unknown-linux-gnu")
            native.assert_not_called()
            command = run.call_args.args[0]
            self.assertEqual(command[command.index("--targets") + 1], "aarch64-unknown-linux-musl")
            self.assertEqual(command[command.index("--linux-builder") + 1], "docker")
            self.assertEqual(command[command.index("--docker-rust-image") + 1], image)
            self.assertEqual(stage.call_args.args[0].name, "hashtree-aarch64-unknown-linux-musl.tar.gz")

    def test_windows_readme_and_encoding_match_the_previous_workflow(self):
        previous = (ROOT / "rust/scripts/fixtures/official_windows_package.ps1").read_text()
        shared = (ROOT / "rust/scripts/package_release_windows.ps1").read_text()
        def readme(script):
            return script.split('@"\n', 1)[1].split('"@ | Out-File', 1)[0]
        self.assertEqual(readme(shared), readme(previous))
        self.assertIn('-Encoding UTF8', previous)
        self.assertIn('-Encoding UTF8', shared)
        self.assertIn('package_release_windows.ps1 text eol=lf', (ROOT / ".gitattributes").read_text())

    @unittest.skipUnless(os.name == "nt", "requires Windows; no compiler needed")
    def test_windows_shared_packager_matches_the_previous_official_payload(self):
        self.assertIsNotNone(shutil.which("pwsh"), "Windows fixture requires PowerShell 7")
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary)
            results = []
            for label in ("old", "new"):
                root = work / label
                binaries = root / "target/x86_64-pc-windows-msvc/release"
                binaries.mkdir(parents=True)
                for name in ("htree.exe", "htree-cashu.exe", "git-remote-htree.exe"):
                    (binaries / name).write_bytes(("fixture " + name).encode())
                if label == "old":
                    body = '$Target = "x86_64-pc-windows-msvc"\n' + (ROOT / "rust/scripts/fixtures/official_windows_package.ps1").read_text()
                    script = root / "previous.ps1"
                    with script.open("w", newline="\n") as file:
                        file.write(body)
                else:
                    script = ROOT / "rust/scripts/package_release_windows.ps1"
                subprocess.run(["pwsh", "-NoProfile", "-File", str(script)], cwd=root, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                with zipfile.ZipFile(root / "dist/release-artifacts/hashtree-x86_64-pc-windows-msvc.zip") as bundle:
                    results.append({member.filename: (bundle.read(member), member.external_attr)
                                    for member in bundle.infolist() if not member.is_dir()})
            self.assertEqual(results[0], results[1])


if __name__ == "__main__":
    unittest.main()
