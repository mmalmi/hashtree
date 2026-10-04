#!/usr/bin/env python3
"""Required public-provider gate against a separately built fixed-core transit."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import time


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--transit", type=Path, required=True)
parser.add_argument("--transit-sha256", required=True)
parser.add_argument("--output", type=Path, required=True)
args = parser.parse_args()
workspace = Path(__file__).resolve().parent.parent
output = args.output.resolve()
output.mkdir(parents=True, exist_ok=True)
assert not (output / "receipt.json").exists(), "Preserve previous gate evidence"
assert sha256(args.transit) == args.transit_sha256, "Transit binary hash mismatch"
transit = output / "discovery_transit_fixture"
shutil.copy2(args.transit, transit)
record = {
    "source": subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=workspace, text=True
    ).strip(),
    "cargoLockSha256": sha256(workspace / "Cargo.lock"),
    "transitSha256": sha256(transit),
    "queryDeadlineSeconds": 8,
    "admittedClients": 16,
    "overflowClients": 1,
    "providerStartRounds": 2,
    "discoveryForwardMinIntervalSeconds": 2,
}
test_source = workspace / "crates/hashtree-cli/src/fips_transport/tests/public_provider.rs"
record["testSourceSha256"] = sha256(test_source)
env = dict(os.environ, CARGO_INCREMENTAL="0", HTREE_TEST_TRANSIT_BIN=str(transit))
command = [
    "cargo", "test", "--locked", "--offline", "-p", "hashtree-cli", "--lib",
    "--features", "public-provider-stress", "--jobs", "1", "--no-run",
    "--message-format=json",
]
record["buildCommand"] = command
with (output / "build.jsonl").open("w") as stdout, (output / "build.log").open("w") as stderr:
    record["buildExitCode"] = subprocess.call(command, cwd=workspace, env=env, stdout=stdout, stderr=stderr)
record["buildLogSha256"] = sha256(output / "build.log")
if record["buildExitCode"] == 0:
    artifacts = [json.loads(line) for line in (output / "build.jsonl").read_text().splitlines()]
    binaries = [
        item["executable"] for item in artifacts
        if item.get("reason") == "compiler-artifact"
        and item.get("target", {}).get("name") == "hashtree_cli"
        and item.get("profile", {}).get("test") and item.get("executable")
    ]
    assert len(binaries) == 1, binaries
    record["testBinarySha256"] = sha256(Path(binaries[0]))
    command = [
        "/usr/bin/time", "-l" if platform.system() == "Darwin" else "-v", binaries[0],
        "fips_transport::tests::public_provider::sixteen_unknown_clients_replay_retained_root_through_transit_after_restart",
        "--exact", "--nocapture", "--test-threads=1",
    ]
    record["testCommand"] = command
    started = time.monotonic()
    with (output / "test.log").open("w") as stdout, (output / "resources.log").open("w") as stderr:
        record["exitCode"] = subprocess.call(command, cwd=workspace, env=env, stdout=stdout, stderr=stderr)
    record["elapsedSeconds"] = time.monotonic() - started
    record["exactTestPassed"] = "test result: ok. 1 passed; 0 failed; 0 ignored;" in (
        output / "test.log"
    ).read_text()
    if record["exitCode"] == 0 and not record["exactTestPassed"]:
        record["exitCode"] = 1
    resources = (output / "resources.log").read_text()
    record["resourceScope"] = "time maximum resident set; not summed process-tree memory"
    match = re.search(r"(\d+)\s+maximum resident set size", resources)
    if match:
        record["peakResidentBytes"] = int(match[1])
    else:
        match = re.search(r"Maximum resident set size \(kbytes\): (\d+)", resources)
        if match:
            record["peakResidentBytes"] = int(match[1]) * 1024
    record["testLogSha256"] = sha256(output / "test.log")
    record["resourceLogSha256"] = sha256(output / "resources.log")
record["lockUnchanged"] = record["cargoLockSha256"] == sha256(workspace / "Cargo.lock")
assert record["lockUnchanged"], "Locked provider/client dependencies changed"
(output / "receipt.json").write_text(json.dumps(record, indent=2) + "\n")
print(json.dumps(record, indent=2))
raise SystemExit(record.get("exitCode", record["buildExitCode"]))
