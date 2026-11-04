#!/usr/bin/env python3
"""Qualify exact draft assets. Published-install acceptance is a separate gate."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tarfile
import tomllib


REPOSITORY = "mmalmi/hashtree"
CANDIDATE = "f19cb5d0cf78e61533cd086f29ba83d7f0193b27"
TAG = "v0.3.1"
RELEASE_ID = 434388035
COMPONENTS = {"hashtree-cli": "0.3.1", "git-remote-htree": "0.3.0",
              "hashtree-core": "0.3.0", "hashtree-cashu-cli": "0.3.0"}
TARGETS = ("x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl",
           "x86_64-apple-darwin", "aarch64-apple-darwin", "x86_64-pc-windows-msvc")
ARCHIVES = {f"hashtree-{target}." + ("zip" if target.endswith("msvc") else "tar.gz")
            for target in TARGETS}
REQUIRED = ARCHIVES | {"install.sh", "build-inputs.json", "notes.md", "release.json"}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def command(arguments, **kwargs):
    result = subprocess.run(arguments, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            check=False, text=True, **kwargs)
    require(result.returncode == 0, f"Read-only qualification command failed with exit {result.returncode}")
    return result.stdout


def api(endpoint, environment):
    return json.loads(command(["gh", "api", "--hostname", "github.com", endpoint], env=environment))


def release_snapshot(environment):
    release = api(f"repos/{REPOSITORY}/releases/{RELEASE_ID}", environment)
    require(release["id"] == RELEASE_ID, "Release identity changed")
    require(release["draft"] is True and release["tag_name"] == TAG and not release["prerelease"],
            "Expected the stable v0.3.1 draft")
    assets = release["assets"]
    require(len({asset["name"] for asset in assets}) == len(assets), "Duplicate draft asset names")
    require({asset["name"] for asset in assets} == REQUIRED, "Draft asset set is incomplete or unexpected")
    require(api(f"repos/{REPOSITORY}/commits/{TAG}", environment)["sha"] == CANDIDATE,
            "Product tag no longer identifies the expected source")
    projected = [{key: asset[key] for key in ("id", "name", "size", "digest", "updated_at")}
                 for asset in assets]
    return {"id": release["id"], "updated_at": release["updated_at"],
            "assets": sorted(projected, key=lambda asset: asset["name"])}


def download_assets(snapshot, destination, environment):
    results = []
    for asset in snapshot["assets"]:
        name = asset["name"]
        require(asset["size"] > 0 and re.fullmatch(r"sha256:[0-9a-f]{64}", asset["digest"] or ""),
                f"Missing size or GitHub digest for {name}")
        path = destination / name
        with path.open("xb") as output:
            result = subprocess.run([
                "gh", "api", "--hostname", "github.com", "--header", "Accept: application/octet-stream",
                f"repos/{REPOSITORY}/releases/assets/{asset['id']}",
            ], env=environment, stdout=output, stderr=subprocess.PIPE, check=False)
        require(result.returncode == 0, f"Draft asset download failed for {name}")
        actual = digest(path)
        require(path.stat().st_size == asset["size"] and "sha256:" + actual == asset["digest"],
                f"Draft asset size or hash mismatch for {name}")
        attestation = command([
            "gh", "attestation", "verify", str(path), "--repo", REPOSITORY,
            "--signer-workflow", REPOSITORY + "/.github/workflows/release.yml",
            "--signer-digest", CANDIDATE, "--source-digest", CANDIDATE,
            "--source-ref", "refs/tags/" + TAG, "--deny-self-hosted-runners", "--format", "json",
        ], env=environment)
        verified = json.loads(attestation)
        require(isinstance(verified, list) and verified, f"No verified attestation for {name}")
        results.append({"name": name, "size": asset["size"], "sha256": actual,
                        "verified_attestations": len(verified),
                        "verification_result_sha256": hashlib.sha256(attestation.encode()).hexdigest()})
    return results


def verify_manifest(assets, source):
    manifest = json.loads((assets / "release.json").read_text())
    require(manifest["tag"] == TAG and manifest["commit"] == CANDIDATE, "Manifest source or tag differs")
    require(manifest["components"] == COMPONENTS, "Manifest component versions differ")
    for name, version in COMPONENTS.items():
        package = tomllib.loads((source / "rust/crates" / name / "Cargo.toml").read_text())["package"]
        require(package["name"] == name and package["version"] == version, f"Source component mismatch for {name}")
    entries = manifest["assets"]
    require(len({entry["name"] for entry in entries}) == len(entries), "Duplicate manifest entries")
    require({entry["name"] for entry in entries} == REQUIRED - {"release.json", "notes.md"},
            "Manifest asset set differs")
    for entry in entries:
        name = entry["name"]
        require(entry["path"] == (name if name == "install.sh" else "assets/" + name),
                f"Unexpected manifest path for {name}")
        require(entry["size"] == (assets / name).stat().st_size and entry["sha256"] == digest(assets / name),
                f"Manifest size or hash mismatch for {name}")
    require(manifest["notes_file"] == "notes.md", "Unexpected release notes path")
    require((assets / "build-inputs.json").read_bytes() == (source / "rust/release-inputs.json").read_bytes(),
            "Published input lock differs from the exact candidate")


def linux_draft_install(assets, working):
    require(platform.system() == "Linux" and platform.machine() == "x86_64", "Expected hosted Linux x86_64")
    extracted, installed = working / "extracted", working / "installed"
    extracted.mkdir()
    installed.mkdir()
    names = ("htree", "htree-cashu", "git-remote-htree")
    with tarfile.open(assets / "hashtree-x86_64-unknown-linux-musl.tar.gz", "r:gz") as archive:
        for name in (*names, "install.sh"):
            matches = [member for member in archive.getmembers() if member.name == "hashtree/" + name]
            require(len(matches) == 1 and matches[0].isfile(), f"Missing unique regular archive member {name}")
            (extracted / name).write_bytes(archive.extractfile(matches[0]).read())
            (extracted / name).chmod(0o755)
    before = {name: digest(extracted / name) for name in names}
    config, data = working / "config", working / "data"
    for directory in (config, data):
        directory.mkdir()
    (config / "config.toml").write_text(
        "[updater]\nauto_check = false\n[nostr]\nenabled = false\nrelays = []\n"
        "[server]\nenable_fips = false\nenable_fips_lan_discovery = false\n"
    )
    # The draft executables receive an isolated environment without Actions/GitHub credentials.
    environment = {"PATH": str(installed) + ":/usr/bin:/bin", "TMPDIR": str(working),
                   "LANG": "C.UTF-8", "HTREE_CONFIG_DIR": str(config), "HTREE_DATA_DIR": str(data),
                   "NOSTR_RELAYS": "", "NOSTR_PREFER_LOCAL": "0", "RUST_LOG": "warn",
                   "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null"}
    command(["bash", "install.sh", str(installed)], cwd=extracted, env=environment, timeout=60)
    require({name: digest(installed / name) for name in names} == before, "Installed draft bytes changed")
    htree, cashu, helper = [str(installed / name) for name in names]
    require(command([htree, "--version"], env=environment, timeout=60).strip() == "htree 0.3.1", "CLI version differs")
    require("Usage:" in command([cashu, "--help"], env=environment, timeout=60), "Cashu help failed")
    capabilities = json.loads(command([helper, "--publisher-capabilities"], env=environment, timeout=60))
    require(capabilities["version"] == "0.3.0" and "complete-blob-dag-v1" in capabilities["capabilities"],
            "Git helper version or capabilities differ")
    protocol = command([helper, "origin", "htree://self/draft-smoke"], input="capabilities\n\n",
                       env=environment, timeout=60)
    require({"fetch", "push", "option"}.issubset(protocol.splitlines()), "Git helper protocol failed")
    command([htree, "stats"], env=environment, timeout=60)
    sample = working / "sample.txt"
    payload = "Hashtree draft archive installation proof\n"
    sample.write_text(payload)
    result = command([htree, "add", "--unencrypted", str(sample)], env=environment, timeout=60)
    root = re.search(r"^\s*url:\s*(\S+)\s*$", result, re.MULTILINE)
    require(root, "Local storage add returned no content address")
    require(command([htree, "cat", root.group(1)], env=environment, timeout=60) == payload, "Storage round trip failed")
    return {"scope": "DRAFT extracted Linux x86_64 archive installation", "installed_sha256": before,
            "checks": ["installed bytes", "CLI 0.3.1", "helper 0.3.0 capabilities",
                       "Cashu startup", "Git helper protocol", "local unencrypted storage round trip"]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--phase", choices=("assets", "install"), required=True)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    require(command(["git", "-C", str(args.source), "rev-parse", "HEAD"]).strip() == CANDIDATE,
            "Qualification checkout is not the exact public candidate")
    assets = args.output.parent / "assets"
    if args.phase == "install":
        require(not os.environ.get("GH_TOKEN") and not os.environ.get("GITHUB_TOKEN"),
                "Draft execution must run in a separate step without the GitHub token")
        receipt = json.loads(args.output.read_text())
        require(receipt["status"] == "draft_assets_verified" and receipt["source"] == CANDIDATE
                and receipt["tag"] == TAG, "Expected the preceding exact draft verification receipt")
        require({asset["name"] for asset in receipt["assets"]} == REQUIRED, "Receipt assets differ")
        for asset in receipt["assets"]:
            path = assets / asset["name"]
            require(path.stat().st_size == asset["size"] and digest(path) == asset["sha256"],
                    "Verified assets changed before isolated installation")
        working = args.output.parent / "linux-install"
        working.mkdir()
        receipt["linux_install"] = linux_draft_install(assets, working)
        receipt["status"] = "passed"
        args.output.write_text(json.dumps(receipt, indent=2) + "\n")
        print(f"DRAFT {TAG} extracted Linux archive installation passed")
        return
    environment = dict(os.environ)
    environment.pop("GH_DEBUG", None)
    environment.pop("DEBUG", None)
    environment["GH_HOST"] = "github.com"
    require(environment.get("GH_TOKEN"), "A read-only GitHub token is required for draft reads")
    before = release_snapshot(environment)
    assets.mkdir(parents=True)
    expected_bytes = sum(asset["size"] for asset in before["assets"])
    largest_asset = max(asset["size"] for asset in before["assets"])
    required_free = expected_bytes + 10 * largest_asset + 1024**3
    available = shutil.disk_usage(assets).free
    require(available >= required_free, "Insufficient hosted capacity for verified assets, extraction and reserve")
    print(f"Draft qualification capacity: {available} bytes available, {required_free} required")
    results = download_assets(before, assets, environment)
    verify_manifest(assets, args.source)
    require(release_snapshot(environment) == before, "Draft changed during asset qualification")
    receipt = {"status": "draft_assets_verified", "scope": "DRAFT asset and extracted-archive qualification",
               "repository": REPOSITORY, "tag": TAG, "source": CANDIDATE, "release_id": before["id"],
               "draft_snapshot": before,
               "gh_version": command(["gh", "--version"]).splitlines()[0], "assets": results,
               "attestation_policy": {"signer_workflow": REPOSITORY + "/.github/workflows/release.yml",
                                      "signer_digest": CANDIDATE, "source_digest": CANDIDATE,
                                      "source_ref": "refs/tags/" + TAG, "deny_self_hosted_runners": True},
               "linux_install": "pending in the token-free step",
               "public_immutable_install_gate": "pending after promotion"}
    args.output.write_text(json.dumps(receipt, indent=2) + "\n")
    print(f"DRAFT {TAG} at {CANDIDATE}; {len(results)} assets verified before installation")


if __name__ == "__main__":
    main()
