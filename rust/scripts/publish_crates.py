"""Publish workspace crates in dependency order, resuming an interrupted release."""
import argparse
from graphlib import TopologicalSorter
import hashlib
import io
import json
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile
from urllib.error import HTTPError
import urllib.request


def registry_read(url):
    request = urllib.request.Request(url, headers={"User-Agent": "hashtree-crate-publisher"})
    with urllib.request.urlopen(request, timeout=30) as response:
        return response.read()


def registry_checksum(name, version):
    lower = name.lower()
    prefix = (str(len(lower)) if len(lower) <= 2 else
              f"3/{lower[0]}" if len(lower) == 3 else f"{lower[:2]}/{lower[2:4]}")
    try:
        index = registry_read(f"https://index.crates.io/{prefix}/{lower}")
    except HTTPError as error:
        if error.code == 404:
            return None
        raise
    versions = [entry for line in index.splitlines() if line
                for entry in [json.loads(line)] if entry["vers"] == version]
    if not versions:
        return None
    if len(versions) != 1 or versions[0]["name"] != name or versions[0]["yanked"] is not False:
        raise ValueError(f"ambiguous, incorrect or yanked index record for {name}@{version}")
    checksum = versions[0]["cksum"]
    if not isinstance(checksum, str) or not re.fullmatch("[0-9a-f]{64}", checksum):
        raise ValueError(f"invalid registry checksum for {name}@{version}")
    return checksum


def package_members(data, root):
    """Compare the files Cargo ships, including executable modes and generated locks."""
    members = {}
    revision = None
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        for member in archive:
            if not member.name.startswith(root + "/"):
                raise ValueError(f"unexpected path in {root} package")
            path = member.name[len(root) + 1:]
            if path in members or any(part in ("", ".", "..") for part in path.split("/")):
                raise ValueError(f"invalid or duplicate member in {root} package")
            if member.isdir():
                members[path] = ("directory", member.mode)
                continue
            if not member.isfile():
                raise ValueError(f"unsupported archive member {path}")
            with archive.extractfile(member) as content:
                body = content.read()
            if path == ".cargo_vcs_info.json":
                # Cargo records the workspace checkout, even for a crate whose
                # inputs were carried forward byte-for-byte from an earlier tag.
                vcs = json.loads(body)
                revision = vcs["git"].pop("sha1")
                if not isinstance(revision, str) or not re.fullmatch("[0-9a-f]{40}", revision) or vcs["git"].get("dirty", False):
                    raise ValueError(f"invalid or dirty Cargo VCS record for {root}")
                body = json.dumps(vcs, sort_keys=True).encode()
            members[path] = ("file", member.mode, hashlib.sha256(body).digest())
    if "Cargo.toml" not in members:
        raise ValueError(f"missing packaged manifest in {root}")
    return members, revision


def verified_existing(name, version, package_dir):
    spec = f"{name}@{version}"
    checksum = registry_checksum(name, version)
    if checksum is None:
        return False
    root = f"{name}-{version}"
    local = (package_dir / f"{root}.crate").read_bytes()
    if hashlib.sha256(local).hexdigest() == checksum:
        print(f"{spec} already published with matching SHA-256 {checksum}")
        return True
    public = registry_read(f"https://static.crates.io/crates/{name}/{root}.crate")
    if hashlib.sha256(public).hexdigest() != checksum:
        raise ValueError(f"registry download checksum mismatch for {spec}")
    wanted, current_revision = package_members(local, root)
    existing, published_revision = package_members(public, root)
    if wanted != existing:
        paths = sorted(path for path in wanted.keys() | existing.keys() if wanted.get(path) != existing.get(path))
        raise ValueError(f"{spec} already exists with different package contents: {', '.join(paths[:10])}")
    print(f"{spec} retaining registry SHA-256 {checksum}; packaged files match "
          f"(registry checkout {published_revision}, candidate checkout {current_revision})")
    return True


def publish_release(plan, crates, flags):
    # Stage all packages together so Cargo can resolve dependencies which have
    # yet to be published. A stale target/package archive must never be evidence.
    with tempfile.TemporaryDirectory(prefix="hashtree-publish-") as target:
        subprocess.run(["cargo", "package", "--locked", "--no-verify", "--registry", "crates-io",
                        "--target-dir", target] + [arg for name in plan for arg in ("-p", name)],
                       check=True)
        package_dir = Path(target) / "package"
        pending = []
        for name in plan:
            version = crates[name]["version"]
            if not verified_existing(name, version, package_dir):
                pending.append((name, version))

        for name, version in pending:
            spec = f"{name}@{version}"
            print(f"Publishing {spec}", flush=True)
            result = subprocess.run(flags + ["-p", name], stdout=subprocess.PIPE,
                                    stderr=subprocess.STDOUT, text=True)
            if result.returncode:
                duplicate = rf"^error: crate {re.escape(spec)} already exists on crates\.io index$"
                if re.search(duplicate, result.stdout, re.MULTILINE) and verified_existing(name, version, package_dir):
                    continue
                sys.exit(result.stdout)
            print(result.stdout, end="")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--plan", action="store_true")
    mode.add_argument("--dry-run", action="store_true")
    parser.add_argument("--version", metavar="TAG", help="Expected CLI release tag")
    args = parser.parse_args()
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"]
    ))
    crates = {p["name"]: p for p in metadata["packages"]
              if p["publish"] is None or "crates-io" in p["publish"]}
    version = crates["hashtree-cli"]["version"]
    if args.version and args.version != "v" + version:
        parser.error(f"requested {args.version}, but hashtree-cli is v{version}")

    graph = {name: sorted({d["name"] for d in p["dependencies"]
                           if d.get("path") and d["name"] in crates
                           and (d["kind"] != "dev" or d["req"] != "*")})
             for name, p in sorted(crates.items())}
    plan = list(TopologicalSorter(graph).static_order())
    if args.plan:
        print("\n".join(plan))
        return

    flags = ["cargo", "publish", "--color", "never", "--locked", "--registry", "crates-io"]
    if args.dry_run:
        # Cargo stages the unpublished dependencies together for verification.
        subprocess.run(flags + ["--dry-run", "--allow-dirty"] +
                       [arg for name in plan for arg in ("-p", name)], check=True)
        return

    try:
        publish_release(plan, crates, flags)
    except (OSError, ValueError, KeyError, tarfile.TarError) as error:
        sys.exit(f"Cannot verify registry release: {error}")


if __name__ == "__main__":
    main()
