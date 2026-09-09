#!/usr/bin/env python3
"""Install exact published bytes in a temporary directory, without rebuilding."""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile
import urllib.request
import zipfile


def download(url):
    request = urllib.request.Request(url, headers={"User-Agent": "hashtree-release-check"})
    with urllib.request.urlopen(request, timeout=120) as response:
        return response.read()


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    assert re.fullmatch(r"v\d+\.\d+\.\d+", args.tag), "expected a stable release tag"
    assert re.fullmatch(r"[0-9a-f]{40}", args.commit), "expected an exact source commit"
    assert args.base_url.startswith("https://"), "expected a published HTTPS URL"

    release = json.loads(download(
        f"https://api.github.com/repos/mmalmi/hashtree/releases/tags/{args.tag}"
    ))
    assert release["immutable"] and not release["draft"], "release must be immutable and public"
    assert release["tag_name"] == args.tag
    assets = {asset["name"]: asset for asset in release["assets"]}

    def verified_download(name, relative_path):
        data = download(f"{args.base_url.rstrip('/')}/{relative_path}")
        asset = assets[name]
        assert len(data) == asset["size"], f"wrong size: {name}"
        assert asset["digest"] == f"sha256:{sha256(data)}", f"wrong digest: {name}"
        return data

    # Bind the canonical download paths to the immutable GitHub manifest and
    # archive digests, including the requested source commit and release tag.
    manifest = json.loads(verified_download("release.json", "release.json"))
    assert manifest["tag"] == args.tag and manifest["commit"] == args.commit
    architecture = "aarch64" if platform.machine().lower() in ("arm64", "aarch64") else "x86_64"
    system = platform.system()
    suffix = {"Linux": "unknown-linux-musl", "Darwin": "apple-darwin", "Windows": "pc-windows-msvc"}[system]
    extension = "zip" if system == "Windows" else "tar.gz"
    archive_name = f"hashtree-{architecture}-{suffix}.{extension}"
    entry = next(asset for asset in manifest["assets"] if asset["name"] == archive_name)
    assert entry["path"] == f"assets/{archive_name}"
    archive = verified_download(archive_name, entry["path"])

    with tempfile.TemporaryDirectory(prefix="hashtree-published-install-") as directory:
        work = Path(directory)
        extracted, installed = work / "extracted", work / "installed"
        extracted.mkdir()
        installed.mkdir()
        names = [name + (".exe" if system == "Windows" else "")
                 for name in ("htree", "htree-cashu", "git-remote-htree")]
        selected = names if system == "Windows" else names + ["install.sh"]
        # Read only known regular files; archive paths never select output paths.
        if system == "Windows":
            with zipfile.ZipFile(io.BytesIO(archive)) as package:
                for name in selected:
                    (extracted / name).write_bytes(package.read(f"hashtree/{name}"))
        else:
            with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as package:
                for name in selected:
                    member = package.getmember(f"hashtree/{name}")
                    assert member.isfile(), f"expected a regular file: {name}"
                    (extracted / name).write_bytes(package.extractfile(member).read())
                    (extracted / name).chmod(0o755)
        binary_hashes = {name: sha256((extracted / name).read_bytes()) for name in names}
        config, data = work / "config", work / "data"
        config.mkdir()
        (config / "config.toml").write_text(
            "[updater]\nauto_check = false\n[nostr]\nenabled = false\nrelays = []\n"
            "[server]\nenable_fips = false\nenable_fips_lan_discovery = false\n"
        )
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("HTREE_", "NOSTR_", "GIT_"))}
        env.update(HTREE_CONFIG_DIR=str(config), HTREE_DATA_DIR=str(data),
                   NOSTR_RELAYS="", NOSTR_PREFER_LOCAL="0", RUST_LOG="warn")

        def run(command, input_text=None, cwd=work):
            return subprocess.run(command, cwd=cwd, env=env, input=input_text,
                                  text=True, capture_output=True, check=True, timeout=60).stdout

        if system == "Windows":
            # The documented Windows installation is extraction into a PATH directory.
            for name in names:
                shutil.copyfile(extracted / name, installed / name)
        else:
            run(["bash", "install.sh", str(installed)], cwd=extracted)
        env["PATH"] = str(installed) + os.pathsep + env["PATH"]
        for name, digest in binary_hashes.items():
            assert sha256((installed / name).read_bytes()) == digest, f"installed bytes changed: {name}"
        htree, cashu, helper = [str(installed / name) for name in names]
        assert run([htree, "--version"]).strip() == f"htree {args.tag[1:]}"
        assert "Usage:" in run([cashu, "--help"])
        capabilities = run([helper, "origin", "htree://self/release-smoke"], "capabilities\n\n")
        assert {"fetch", "push", "option"}.issubset(capabilities.splitlines())
        run([htree, "stats"])
        payload = "Hashtree published installation round trip\n"
        sample = work / "sample.txt"
        sample.write_text(payload)
        added = run([htree, "add", "--unencrypted", str(sample)])
        root = re.search(r"^\s*url:\s*(\S+)\s*$", added, re.MULTILINE)
        assert root, "add did not return a content address"
        assert run([htree, "cat", root.group(1)]) == payload

    receipt = {"tag": args.tag, "commit": args.commit, "platform": system,
               "architecture": architecture, "archive": archive_name,
               "archive_sha256": sha256(archive), "installed_sha256": binary_hashes,
               "checks": ["immutable manifest", "published archive", "installed bytes",
                          "CLI version", "Cashu startup", "Git helper", "storage round trip"]}
    Path(args.output).write_text(json.dumps(receipt, indent=2) + "\n")
    print(f"Published installation passed: {args.tag} {architecture} {system}")


if __name__ == "__main__":
    main()
