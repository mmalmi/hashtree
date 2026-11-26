#!/usr/bin/env python3
"""Rebuild the complete Haps-mapped release payload using the release builders.

Requires the native OS/SDK recorded in release-inputs.json, or Docker for Linux.
Use Haps's explicit host-build mode; this script never changes build permissions.
An actual payload comparison is still required to claim binary reproducibility.
Haps creates a fresh source checkout for each build/audit invocation. Retry through
Haps after a failure; this adapter uses fixed output paths within that checkout
and intentionally refuses to overwrite them or create another large cache.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import stat
import subprocess
import tarfile
import zipfile


TARGETS = {
    "x86_64-unknown-linux-gnu": "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-gnu": "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin": "x86_64-apple-darwin",
    "aarch64-apple-darwin": "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc": "x86_64-pc-windows-msvc",
}


def stage_archive(archive, destination):
    """Apply the same regular-file/mode mapping as Haps import-release."""
    destination.mkdir()
    seen = set()

    def write(name, mode, stream):
        member = PurePosixPath(name)
        if (member.is_absolute() or ".." in member.parts or "\\" in name
                or ":" in name or not member.parts or member.parts[0] != "hashtree"
                or len(member.parts) < 2 or any(ord(c) < 32 for c in name)):
            raise ValueError("Release member escapes the mapped hashtree root")
        relative = Path(*member.parts[1:])
        if relative in seen:
            raise ValueError("Duplicate release member")
        seen.add(relative)
        output = destination / relative
        output.parent.mkdir(parents=True, exist_ok=True)
        with output.open("xb") as target:
            shutil.copyfileobj(stream, target)
        output.chmod(0o755 if mode & 0o111 else 0o644)

    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as bundle:
            for member in bundle.infolist():
                mode = member.external_attr >> 16
                if stat.S_IFMT(mode) not in (0, stat.S_IFREG, stat.S_IFDIR):
                    raise ValueError("Release contains a link or special file")
                if member.is_dir():
                    continue
                with bundle.open(member) as stream:
                    write(member.filename, mode, stream)
    else:
        with tarfile.open(archive, "r:*") as bundle:
            for member in bundle:
                if member.isdir():
                    continue
                if not member.isfile():
                    raise ValueError("Release contains a link or special file")
                with bundle.extractfile(member) as stream:
                    write(member.name, member.mode, stream)
    if not seen:
        raise ValueError("Release payload is empty")


def prepare_native(root, work, target):
    """Reuse the checked/download-verified release setup in this process."""
    specification = importlib.util.spec_from_file_location(
        "release_inputs", root / "rust/scripts/release_inputs.py")
    inputs = importlib.util.module_from_spec(specification)
    specification.loader.exec_module(inputs)
    runner = work / "native-inputs"
    runner.mkdir()
    # The existing setup exports CI variables; direct them only to this build.
    os.environ.update(RUNNER_TEMP=str(runner),
                      GITHUB_ENV=str(runner / "environment"),
                      GITHUB_PATH=str(runner / "paths"))
    lock = json.loads((root / "rust/release-inputs.json").read_text())
    # Match release_inputs.py native: host selects the compiler/SDK archive;
    # target may select the other architecture's standard library on macOS.
    architecture = "aarch64" if platform.machine().lower() in ("arm64", "aarch64") else "x86_64"
    suffix = "apple-darwin" if platform.system() == "Darwin" else "pc-windows-msvc"
    inputs.install_native(lock, architecture + "-" + suffix, target)
    # GitHub applies GITHUB_PATH in the next step. Here the next command is local.
    os.environ["PATH"] = str(runner / "pinned-rust/bin") + os.pathsep + os.environ["PATH"]


def build(root, package_target):
    build_target = TARGETS[package_target]
    expected_os = ("Linux" if "linux" in build_target else
                   "Windows" if "windows" in build_target else "Darwin")
    if platform.system() != expected_os:
        raise ValueError("Run the recipe on the package's native OS")
    version_text = (root / "rust/crates/hashtree-cli/Cargo.toml").read_text()
    version = re.search(r'^version = "(\d+\.\d+\.\d+)"$', version_text, re.M)
    if not version:
        raise ValueError("CLI source must declare a stable release version")
    work = root / "target/haps-release"
    work.mkdir(parents=True, exist_ok=False)
    output = work / "archives"
    binaries = work / "cargo"
    run = lambda command: subprocess.run(command, cwd=root / "rust", check=True)
    if expected_os != "Linux":
        prepare_native(root, work, build_target)
    if expected_os == "Windows":
        run(["cargo", "build", "--release", "--locked", "--target", build_target,
             "--target-dir", str(binaries),
             "-p", "hashtree-cashu-cli", "-p", "hashtree-cli",
             "--features", "hashtree-cli/fips-webrtc"])
        run(["pwsh", "-NoProfile", "-File", "scripts/package_release_windows.ps1",
             "-Target", build_target, "-BinaryDirectory", str(binaries / build_target / "release"),
             "-OutputDirectory", str(output)])
        suffix = "zip"
    else:
        command = ["bash", "scripts/build_release_artifacts.sh", "--version", "v" + version[1],
                   "--targets", build_target, "--target-dir", str(binaries),
                   "--output-dir", str(output)]
        if expected_os == "Linux":
            lock = json.loads((root / "rust/release-inputs.json").read_text())
            image = lock["linux"][build_target]["image"]
            if not re.fullmatch(r"[^\s@]+@sha256:[0-9a-f]{64}", image):
                raise ValueError("Linux release image must be digest pinned")
            command += ["--linux-builder", "docker", "--docker-rust-image", image]
        else:
            command += ["--cargo-bin", str(work / "native-inputs/pinned-rust/bin/cargo")]
        run(command)
        suffix = "tar.gz"
    stage_archive(output / ("hashtree-" + build_target + "." + suffix), root / "target/haps-payload")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=TARGETS, required=True)
    arguments = parser.parse_args()
    build(Path(__file__).resolve().parents[2], arguments.target)
