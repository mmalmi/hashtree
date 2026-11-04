#!/usr/bin/env python3
"""Run upload-size regressions with real helper I/O confined to loopback."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess


def tail(path):
    print("\n".join(path.read_text(errors="replace").splitlines()[-60:]))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--expect-failure", action="store_true")
    parser.add_argument("--logs", required=True, type=Path)
    args = parser.parse_args()
    args.logs.mkdir(parents=True, exist_ok=True)
    if shutil.disk_usage(".").free < 8 * 1024**3:
        raise SystemExit("At least 8 GiB free is required before the hosted build")

    targets = {"directory_upload_sizes": ([], 4), "add_packed_git": ([], 1)}
    command = ["cargo", "test", "--locked", "-p", "hashtree-cli", "--features",
               "git-remote-wrapper", "--no-run", "--message-format=json"]
    for name in targets:
        command += ["--test", name]
    if not args.expect_failure:
        command += ["--test", "storage_streaming", "--lib"]
        targets["storage_streaming"] = ([], 7)
        targets["hashtree_cli"] = (["storage::add_cache::tests"], 3)

    artifacts = args.logs / "build.jsonl"
    diagnostics = args.logs / "build.log"
    with artifacts.open("w") as stdout, diagnostics.open("w") as stderr:
        result = subprocess.run(command, stdout=stdout, stderr=stderr, timeout=1500)
    if result.returncode:
        tail(diagnostics)
        for line in artifacts.read_text().splitlines():
            item = json.loads(line)
            if item.get("reason") == "compiler-message":
                print(item["message"].get("rendered", ""))
        raise SystemExit(result.returncode)

    executables = {}
    for line in artifacts.read_text().splitlines():
        item = json.loads(line)
        if item.get("reason") == "compiler-artifact" and item.get("executable"):
            name = item["target"]["name"]
            if item["profile"]["test"] and name in targets:
                executables[name] = item["executable"]
    if executables.keys() != targets.keys():
        raise SystemExit(f"Missing required test binaries: {targets.keys() - executables.keys()}")

    for name, (filters, count) in targets.items():
        log = args.logs / f"{name}.log"
        # Only loopback exists in the namespace. Tests cannot contact public relays,
        # servers, an installed daemon, or external FIPS peers.
        command = [
            "sudo", "unshare", "--net", "bash", "-ec",
            'ip link set lo up\nexec setpriv --reuid="$1" --regid="$2" --init-groups "$3" "${@:4}"',
        ]
        command += ["upload-size-test", str(os.getuid()), str(os.getgid()),
                    executables[name], *filters, "--nocapture", "--test-threads=1"]
        with log.open("w") as output:
            result = subprocess.run(command, stdout=output, stderr=subprocess.STDOUT, timeout=300)
        text = log.read_text(errors="replace")
        expected = (f"test result: FAILED. 0 passed; {count} failed; 0 ignored;"
                    if args.expect_failure else
                    f"test result: ok. {count} passed; 0 failed; 0 ignored;")
        if (result.returncode != 0) != args.expect_failure or expected not in text:
            tail(log)
            raise SystemExit(f"Unexpected regression result for {name}: exit {result.returncode}")
        print(f"{name}: {expected}", flush=True)


if __name__ == "__main__":
    main()
