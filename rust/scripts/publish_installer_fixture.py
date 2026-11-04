#!/usr/bin/env python3
"""Publish only the fixed public installer fixture after exact draft qualification.

Run in a token-free hosted step. This produces a public relay envelope for the
independent site verifier; it does not assert signature or clone acceptance.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import signal
import stat
import subprocess
import tempfile

CANDIDATE = "f19cb5d0cf78e61533cd086f29ba83d7f0193b27"
RELAYS = ["wss://upload.iris.to/ws", "wss://cdn.iris.to/ws",
          "wss://temp.iris.to", "wss://nos.lol", "wss://relay.snort.social"]
READ_SERVERS = ["https://upload.iris.to", "https://cdn.iris.to"]
WRITE_SERVERS = ["https://upload.iris.to"]
SOURCE = "https://github.com/mmalmi/hashtree-spec.git"
HEAD = "4811761e9378b1ef877565c47e0f2a428d2a7dbe"
TREE = "73cd2980ff86e52cb4cc01e79d2b5daaf633afc2"
HISTORY = [HEAD, "8d319e83d1dd7728fb1367c3ed58df5be941d573",
           "b36accfb1e533b3c3776379a848fe505d59221e1"]
SERIALIZED_SHA256 = "f8537018128af48c4f85188bc8f47020d5708c90098007abbc7d27e6f0e29bc1"


def require(value, message):
    if not value:
        raise ValueError(message)


def sha256(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def run(stage, args, *, env, cwd=None, data=None, timeout=60, evidence=None):
    child = subprocess.Popen(args, env=env, cwd=cwd, stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             start_new_session=True)
    try:
        stdout, stderr = child.communicate(data, timeout=timeout)
    except subprocess.TimeoutExpired:
        # Stop the entire Git/helper group before disposing its ephemeral identity.
        os.killpg(child.pid, signal.SIGTERM)
        try:
            child.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.communicate()
        raise RuntimeError(stage + " timed out; publication state may be indeterminate") from None
    if evidence is not None:
        details = {"stage": stage, "exit": child.returncode}
        if stage == "Fixture publication":
            text = stderr.decode("utf-8", errors="replace")
            published = re.findall(r"Published to: (htree://npub1[a-z0-9]+/hashtree-installer-fixture-[0-9a-f]{16})", text)
            details["reportedRemoteMatches"] = published == [evidence["publicRemote"]]
            # This operation has only a fresh test identity and the fixed public graph.
            # Retain actual helper errors, removing execution paths and any key-like text.
            errors = re.findall(r"(?:Push to refs/heads/main failed|error|fatal): (.+)", text)
            sanitized = []
            for error in errors[:4]:
                error = error.replace(env.get("TMPDIR", "__no_task_path__"), "<ephemeral-dir>")
                error = re.sub(r"(?:/home/runner|/tmp|/var/tmp)/[^\s,;]+", "<ephemeral-path>", error)
                error = re.sub(r"nsec1[a-z0-9]+", "<redacted-key>", error)
                sanitized.append(error[:2048])
            details["errors"] = sanitized
            for label in ("Relays", "Relays failed", "Blossom", "Blossom failed"):
                lines = re.findall(r"^\s*" + re.escape(label) + r": (.+)$", text, re.MULTILINE)
                allowed = set(RELAYS + READ_SERVERS)
                details[label] = sorted({url.strip() for line in lines for url in line.split(",")
                                         if url.strip() in allowed})
        elif stage == "Public root readback":
            observations = []
            for line in stdout.decode("utf-8", errors="replace").splitlines():
                try:
                    item = json.loads(line)
                except ValueError:
                    continue
                if item.get("relay") in RELAYS and item.get("phase") in ("open", "eose", "error", "closed", "accepted"):
                    observations.append({key: item[key] for key in ("relay", "phase")})
            details["observations"] = observations[:30]
        evidence.setdefault("transportEvidence", []).append(details)
    if child.returncode:
        # Do not copy tool logs, process environment, local paths or identity data
        # into the public receipt. HTTP status classes can identify write refusal.
        codes = sorted(set(re.findall(rb"\b(?:HTTP(?:/[0-9.]+)?|status(?: code)?)[: =]+([45][0-9]{2})\b",
                                      stderr, flags=re.IGNORECASE)))
        suffix = ", HTTP " + ",".join(x.decode() for x in codes) if codes else ""
        raise RuntimeError(f"{stage} failed (exit {child.returncode}{suffix})")
    return stdout


def self_npub(output):
    matches = re.findall(rb"^(npub1[ac-hj-np-z02-9]{58})[ ]+\(self\)$", output, re.MULTILINE)
    require(len(matches) == 1, "Fresh identity did not return one self identity")
    return matches[0].decode()


def npub_hex(npub):
    """Decode the public CLI output with a strict Bech32 checksum, no key access."""
    alphabet = "qpzry9x8gf2tvdw0s3jn54khce6mua7l"
    require(re.fullmatch(r"npub1[ac-hj-np-z02-9]{58}", npub), "Invalid public identity encoding")
    values = [alphabet.index(char) for char in npub[5:]]
    expanded = [ord(c) >> 5 for c in "npub"] + [0] + [ord(c) & 31 for c in "npub"]
    check = 1
    generators = [0x3B6A57B2, 0x26508E6D, 0x1EA119FA, 0x3D4233DD, 0x2A1462B3]
    for value in expanded + values:
        top, check = check >> 25, ((check & 0x1FFFFFF) << 5) ^ value
        for i, generator in enumerate(generators):
            if (top >> i) & 1:
                check ^= generator
    require(check == 1, "Invalid public identity checksum")
    accumulator = bits = 0
    decoded = bytearray()
    for value in values[:-6]:
        accumulator, bits = (accumulator << 5) | value, bits + 5
        while bits >= 8:
            bits -= 8
            decoded.append((accumulator >> bits) & 255)
    require(bits < 5 and ((accumulator << (8 - bits)) & 255) == 0 and len(decoded) == 32,
            "Invalid public identity padding")
    return decoded.hex()


def expected_graph():
    return {"schemaVersion": 1, "sourceRepository": SOURCE[:-4], "expectedHead": HEAD,
            "expectedTree": TREE, "expectedBranch": "main", "expectedHistory": HISTORY,
            "expectedObjectCount": 38, "expectedSerializedBytes": 134181,
            "expectedSerializedSha256": SERIALIZED_SHA256,
            "serialization": "git cat-file --batch, with sorted unique reachable object IDs as input",
            "qualifiedVersionedPublication": False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--qualification-receipt", type=Path, required=True)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--event-reader", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    require(os.environ.get("GITHUB_ACTIONS") == "true" and os.environ.get("RUNNER_OS") == "Linux",
            "Fixture publication is restricted to the hosted Linux qualification job")
    require(not os.environ.get("GH_TOKEN") and not os.environ.get("GITHUB_TOKEN"),
            "Fixture step must not receive a GitHub token")
    require(args.output_dir.is_absolute() and not args.output_dir.exists(), "Output directory must be new")
    args.bin_dir = args.bin_dir.resolve(strict=True)
    args.event_reader = args.event_reader.resolve(strict=True)
    receipt = json.loads(args.qualification_receipt.read_text())
    require(receipt["status"] == "passed" and receipt["source"] == CANDIDATE
            and receipt["tag"] == "v0.3.1" and receipt["repository"] == "mmalmi/hashtree",
            "Exact candidate draft qualification has not passed")
    binary_hashes = receipt["linux_install"]["installed_sha256"]
    require(set(binary_hashes) == {"htree", "htree-cashu", "git-remote-htree"}, "Unexpected installed binaries")
    for name, expected in binary_hashes.items():
        require(re.fullmatch(r"[0-9a-f]{64}", expected) and sha256(args.bin_dir / name) == expected,
                "Qualified installed binary changed")
    git, node = shutil.which("git"), shutil.which("node")
    require(git and node, "Git and Node with built-in WebSocket are required")
    os.umask(0o077)
    args.output_dir.mkdir(mode=0o700)
    public_receipt = {**expected_graph(), "status": "preparing", "binarySource": CANDIDATE,
                      "binaryTag": "v0.3.1", "installedSha256": binary_hashes,
                      "qualificationReceiptSha256": sha256(args.qualification_receipt),
                      "relays": RELAYS, "blossomReadServers": READ_SERVERS,
                      "blossomWriteServers": WRITE_SERVERS,
                      "independentSiteVerification": "pending signature, versioned refs and installed clone"}
    output = args.output_dir / "fixture.json"

    def save():
        output.write_text(json.dumps(public_receipt, indent=2) + "\n")

    save()
    try:
        with tempfile.TemporaryDirectory(prefix="hashtree-installer-identity-", dir=os.environ["RUNNER_TEMP"]) as scratch:
            work = Path(scratch)
            config, data, repository = [work / name for name in ("config", "data", "repo")]
            for directory in (config, data, repository):
                directory.mkdir(mode=0o700)
            environment = {"PATH": str(args.bin_dir) + ":/usr/local/bin:/usr/bin:/bin",
                           "TMPDIR": str(work), "LANG": "C.UTF-8",
                           "HTREE_CONFIG_DIR": str(config), "HTREE_DATA_DIR": str(data),
                           "NOSTR_RELAYS": ",".join(RELAYS), "NOSTR_PREFER_LOCAL": "0",
                           "HTREE_PREFER_LOCAL_DAEMON": "0", "RUST_LOG": "warn",
                           "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
                           "GIT_TERMINAL_PROMPT": "0"}
            # JSON strings/arrays are also valid for these TOML values.
            (config / "config.toml").write_text(
                "[storage]\ndata_dir = " + json.dumps(str(data)) + "\nmax_size_gb = 1\n"
                + "[updater]\nauto_check = false\nauto_install = false\n"
                + "[server]\nenable_fips = false\nenable_fips_lan_discovery = false\n"
                + "[nostr]\nrelays = " + json.dumps(RELAYS) + "\nbootstrap_follows = []\n"
                + "[blossom]\nread_servers = " + json.dumps(READ_SERVERS)
                + "\nwrite_servers = " + json.dumps(WRITE_SERVERS) + "\nupload_concurrency = 2\n")
            run("Node capability preflight", [node, "-e", "if(typeof WebSocket !== 'function') process.exit(1)"], env=environment)

            def git_run(*arguments, data=None, timeout=60):
                return run("Public fixture Git operation", [git, *arguments], env=environment,
                           cwd=repository, data=data, timeout=timeout)

            git_run("init", "--initial-branch=main")
            git_run("-c", "credential.helper=", "fetch", "--no-tags", SOURCE, HEAD, timeout=180)
            git_run("checkout", "-B", "main", "FETCH_HEAD")
            require(git_run("rev-parse", "HEAD").decode().strip() == HEAD, "Public fixture head differs")
            require(git_run("rev-parse", "HEAD^{tree}").decode().strip() == TREE, "Public fixture tree differs")
            require(git_run("rev-list", "HEAD").decode().splitlines() == HISTORY, "Public fixture history differs")
            require(git_run("for-each-ref", "--format=%(refname)").decode().splitlines() == ["refs/heads/main"],
                    "Unexpected refs in public fixture")
            objects = sorted(set(git_run("rev-list", "--objects", "--no-object-names", "HEAD").decode().splitlines()))
            require(len(objects) == 38 and all(re.fullmatch(r"[0-9a-f]{40}", item) for item in objects),
                    "Public fixture object set differs")
            serialized = git_run("cat-file", "--batch", data=("\n".join(objects) + "\n").encode())
            require(len(serialized) == 134181 and hashlib.sha256(serialized).hexdigest() == SERIALIZED_SHA256,
                    "Public fixture object bytes differ")

            user = run("Fresh test identity initialization", [str(args.bin_dir / "htree"), "user"], env=environment)
            npub = self_npub(user)
            pubkey = npub_hex(npub)
            key_stat = (config / "keys").stat()
            require(stat.S_ISREG(key_stat.st_mode) and stat.S_IMODE(key_stat.st_mode) == 0o600,
                    "Fresh signing key file permissions are not private")
            name = "hashtree-installer-fixture-" + secrets.token_hex(8)
            public_receipt.update(status="prepared", publicNpub=npub, publicKey=pubkey,
                                  repositoryName=name, publicRemote="htree://" + npub + "/" + name)
            save()  # A public recovery address survives a publication/readback timeout.
            run("Fixture publication", [git, "push", "htree://self/" + name,
                                         "refs/heads/main:refs/heads/main"],
                env=environment, cwd=repository, timeout=600, evidence=public_receipt)
            public_receipt["status"] = "push_completed_readback_pending"
            save()
            event_path = args.output_dir / "root-event.json"
            run("Public root readback", [node, str(args.event_reader), pubkey, name, str(event_path)],
                env=environment, timeout=65, evidence=public_receipt)
            event = json.loads(event_path.read_text())
            tag = lambda key: next(item[1] for item in event["tags"] if item[0] == key)
            public_receipt.update(status="published_envelope_observed", eventId=event["id"],
                                  rootHash=tag("hash"), rootKey=tag("key"),
                                  eventReceiptSha256=sha256(event_path))
            save()
        print(json.dumps({"status": public_receipt["status"], "publicRemote": public_receipt["publicRemote"],
                          "eventId": public_receipt["eventId"], "qualifiedVersionedPublication": False}))
    except Exception as error:
        prior = public_receipt["status"]
        public_receipt.update(status="failed", lastCompletedState=prior, failureType=type(error).__name__)
        # Deliberately exclude arbitrary exception text that could contain private paths.
        save()
        if isinstance(error, RuntimeError):
            print(str(error))  # run() constructs only bounded, non-secret stage/status text.
        else:
            print("Fixture preparation failed validation; public receipt retains the last completed state")
        raise SystemExit(1) from None


if __name__ == "__main__":
    main()
