#!/usr/bin/env python3
"""Compile the actual iOS backend and reject forbidden System V semaphore calls."""

import json
import os
from pathlib import Path
import platform
import subprocess


def main():
    if platform.system() != "Darwin":
        raise SystemExit("This check requires the Apple SDKs on macOS")
    root = Path(__file__).resolve().parents[1]
    env = dict(os.environ)
    env.setdefault("CARGO_BUILD_JOBS", "2")
    host = subprocess.check_output(["rustc", "-vV"], text=True).split("host: ")[1].splitlines()[0]
    targets = ("aarch64-apple-ios", "aarch64-apple-ios-sim")
    for target in (host, *targets):
        graph = subprocess.check_output(
            ["cargo", "tree", "--locked", "-p", "hashtree-lmdb", "--target", target,
             "--edges", "normal,build", "--prefix", "none", "--format", "{p}|{f}"],
            cwd=root, env=env, text=True,
        )
        rows = [line for line in graph.splitlines() if line.startswith("hashtree-lmdb-master-sys ")]
        assert rows, f"missing LMDB dependency for {target}"
        posix = any("posix-sem" in row.split("|", 1)[1] for row in rows)
        assert posix == (target in targets), f"wrong locking features for {target}: {rows}"
    for target in targets:
        result = subprocess.run(
            ["cargo", "build", "--locked", "-p", "hashtree-lmdb", "--lib", "--target", target,
             "--message-format=json-render-diagnostics"],
            cwd=root, env=env, text=True, stdout=subprocess.PIPE, check=True,
        )
        archives = set()
        for line in result.stdout.splitlines():
            event = json.loads(line)
            if event.get("reason") == "build-script-executed" and "hashtree-lmdb-master-sys" in event["package_id"]:
                archives.add(Path(event["out_dir"]) / "liblmdb.a")
        assert len(archives) == 1, f"expected one actual LMDB archive: {archives}"
        archive = archives.pop()
        symbols = subprocess.check_output(["xcrun", "nm", "-u", str(archive)], text=True)
        symbols = {line.split()[-1] for line in symbols.splitlines() if line.split()}
        forbidden = {"_semget", "_semctl", "_semop"}
        required = {"_sem_open", "_sem_wait", "_sem_post", "_sem_close", "_sem_unlink"}
        assert not symbols & forbidden, f"unsupported iOS calls: {symbols & forbidden}"
        assert required <= symbols, f"locking calls missing: {required - symbols}"
        print(f"{target}: POSIX reader/writer locks present; System V calls absent")


if __name__ == "__main__":
    main()
