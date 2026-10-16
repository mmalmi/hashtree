#!/usr/bin/env python3
"""Compile the actual iOS backend and reject forbidden System V semaphore calls."""

import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile


def check_symbols(archive, posix):
    symbols = subprocess.check_output(["xcrun", "nm", "-u", str(archive)], text=True)
    symbols = {line.split()[-1] for line in symbols.splitlines() if line.split()}
    sysv = {"_semget", "_semctl", "_semop"}
    named = {"_sem_open", "_sem_wait", "_sem_post", "_sem_close", "_sem_unlink"}
    required, forbidden = (named, sysv) if posix else (sysv, named)
    assert not symbols & forbidden, f"wrong locking calls: {symbols & forbidden}"
    assert required <= symbols, f"locking calls missing: {required - symbols}"


def build(root, env, args):
    result = subprocess.run(
        ["cargo", *args, "--message-format=json-render-diagnostics"],
        cwd=root, env=env, text=True, stdout=subprocess.PIPE, check=True,
    )
    events = [json.loads(line) for line in result.stdout.splitlines()]
    archives = {Path(e["out_dir"]) / "liblmdb.a" for e in events
                if e.get("reason") == "build-script-executed"
                and "hashtree-lmdb-master-sys" in e["package_id"]}
    assert len(archives) == 1, f"expected one actual LMDB archive: {archives}"
    return events, archives.pop()


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
        _, archive = build(root, env, ["build", "--locked", "-p", "hashtree-lmdb", "--lib", "--target", target])
        check_symbols(archive, posix=True)
        print(f"{target}: POSIX reader/writer locks present; System V calls absent")

    executables = []
    for posix in (False, True):
        args = ["test", "--locked", "-p", "hashtree-lmdb", "--test", "locking_lifecycle", "--no-run"]
        if posix:
            args.extend(["--features", "heed/posix-sem"])
        events, archive = build(root, env, args)
        check_symbols(archive, posix=posix)
        binaries = {e["executable"] for e in events
                    if e.get("reason") == "compiler-artifact" and e.get("executable")
                    and e["target"]["name"] == "locking_lifecycle"}
        assert len(binaries) == 1, f"missing test executable: {binaries}"
        binary = binaries.pop()
        subprocess.run([binary], env=env, check=True, timeout=90)
        executables.append(binary)
    with tempfile.TemporaryDirectory(prefix="hashtree-lock-format-") as directory:
        # A previous SysV process dies while holding a writer lock. POSIX must
        # reopen the same files only after that last owner exits, preserving data.
        for binary, mode in zip(executables, ("crash", "recover")):
            child_env = dict(env, HASHTREE_LOCKING_CHILD=mode, HASHTREE_LOCKING_PATH=directory)
            subprocess.run([binary, "--exact", "locking_child", "--ignored"],
                           env=child_env, check=True, timeout=30)
    print("Previous SysV lock region reopened safely with POSIX; durable ciphertext and pins retained")


if __name__ == "__main__":
    main()
