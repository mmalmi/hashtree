#!/usr/bin/env python3
"""Retrieve exact previously qualified public bytes; never execute with credentials."""
import hashlib, io, json, os, pathlib, shutil, subprocess, tarfile, zipfile

ROOT = pathlib.Path(os.environ["RUNNER_TEMP"]) / "draft-qualification"
RECEIPT_SHA = "d709c63dd128c62ad8cb229ff3f9405847ac3b6a62f8c58097d3f3a6e546698c"
SOURCE = "f19cb5d0cf78e61533cd086f29ba83d7f0193b27"

def api(path, binary=False):
    args = ["gh", "api", "--hostname", "github.com"]
    if binary:
        args += ["--header", "Accept: application/octet-stream"]
    return subprocess.check_output(args + ["repos/mmalmi/hashtree/" + path])

def sha(data):
    return hashlib.sha256(data).hexdigest()

assert not ROOT.exists()
ROOT.mkdir()
assert shutil.disk_usage(ROOT).free >= 1024**3
with zipfile.ZipFile(io.BytesIO(api("actions/artifacts/13357968849/zip"))) as archive:
    assert archive.namelist() == ["receipt.json"]
    assert archive.getinfo("receipt.json").file_size < 100000
    receipt_bytes = archive.read("receipt.json")
assert sha(receipt_bytes) == RECEIPT_SHA
receipt = json.loads(receipt_bytes)
assert receipt["status"] == "passed" and receipt["source"] == SOURCE and receipt["tag"] == "v0.3.1"
assert json.loads(api("commits/v0.3.1"))["sha"] == SOURCE
release = json.loads(api("releases/434388035"))
assert release["id"] == receipt["release_id"] and release["tag_name"] == "v0.3.1"
expected = {a["name"]: a for a in receipt["assets"]}
assert {a["name"] for a in release["assets"]} == set(expected)
for asset in release["assets"]:
    assert asset["size"] == expected[asset["name"]]["size"]
    assert asset["digest"] == "sha256:" + expected[asset["name"]]["sha256"]
asset = next(a for a in release["assets"] if a["name"] == "hashtree-x86_64-unknown-linux-musl.tar.gz")
archive_bytes = api("releases/assets/" + str(asset["id"]), binary=True)
assert len(archive_bytes) == asset["size"] and sha(archive_bytes) == expected[asset["name"]]["sha256"]
installed = ROOT / "linux-install" / "installed"
installed.mkdir(parents=True)
with tarfile.open(fileobj=io.BytesIO(archive_bytes), mode="r:gz") as archive:
    for name, digest in receipt["linux_install"]["installed_sha256"].items():
        assert name in ("htree", "htree-cashu", "git-remote-htree")
        matches = [entry for entry in archive.getmembers() if entry.name == "hashtree/" + name]
        assert len(matches) == 1 and matches[0].isfile()
        binary = archive.extractfile(matches[0]).read()
        assert sha(binary) == digest
        (installed / name).write_bytes(binary)
        (installed / name).chmod(0o755)
(ROOT / "receipt.json").write_bytes(receipt_bytes)
print("Reused exact nine-asset attestation/install receipt and three verified Linux binary hashes")
