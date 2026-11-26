"""Publish workspace crates in dependency order, resuming an interrupted release."""
import argparse
from graphlib import TopologicalSorter
import json
import re
import subprocess
import sys


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

    for name in plan:
        spec = f'{name}@{crates[name]["version"]}'
        print(f"Publishing {spec}", flush=True)
        result = subprocess.run(flags + ["-p", name], stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, text=True)
        if result.returncode:
            # Only an exact registry duplicate is a successful retry.
            duplicate = rf"^error: crate {re.escape(spec)} already exists on crates\.io index$"
            if re.search(duplicate, result.stdout, re.MULTILINE):
                print(f"{spec} already published at this version (skipping)")
                continue
            sys.exit(result.stdout)
        print(result.stdout, end="")


if __name__ == "__main__":
    main()
