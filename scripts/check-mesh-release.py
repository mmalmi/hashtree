#!/usr/bin/env python3
"""Bind a successful Iris Stack gate receipt to this Hashtree release candidate."""

import json
import math
import os
import re
import sys


LAB_REV = "cec9501659b9cf08f9600e3f14987cad6ca1e7d3"


def check(receipt, revision):
    expected = {
        "hashtree": {"source": "https://github.com/mmalmi/hashtree", "rev": revision},
        "chat": {"source": "https://github.com/irislib/iris-chat-rs", "rev": "a4cafb1bb382593c9886d0a4314cf80292ef7850"},
        "drive": {"source": "htree://npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/iris-drive", "rev": "05751a828f2a20b3ed46d09569e6cf35ac4c537d"},
    }
    if not (re.fullmatch(r"[0-9a-f]{40}", revision) and type(receipt.get("schema_version")) is int and receipt["schema_version"] == 1
            and receipt.get("status") == "passed" and receipt.get("lab_revision") == LAB_REV
            and receipt.get("release_gate") is True and receipt.get("lab_worktree_clean") is True):
        raise ValueError("Mesh gate receipt must be a successful release check from the pinned clean lab")
    for name, source in expected.items():
        product = receipt["products"][name]
        if any(product.get(key) != value for key, value in source.items()) or not re.fullmatch(r"[0-9a-f]{64}", product.get("sha256", "")):
            raise ValueError(f"Mesh gate receipt does not match the {name} release source")
    samples = receipt["metrics"]["idle"]
    if len(samples) != 2 or any(sample.get("cpu_required") is not True
            or type(sample.get("seconds")) not in (int, float)
            or not math.isfinite(sample["seconds"]) or sample["seconds"] < 65
            or sample.get("cpu_budget_percent") != 5 or sample.get("wire_budget_bytes_per_second") != 4096
            or len(sample["cpu_percent"]) != 3
            or any(type(cpu) not in (int, float) or not math.isfinite(cpu) or not 0 <= cpu <= 5 for cpu in sample["cpu_percent"])
            or type(sample["combined_bytes_per_second"]) not in (int, float)
            or not 0 <= sample["combined_bytes_per_second"] < 4096 for sample in samples):
        raise ValueError("Mesh gate receipt requires passing CPU and bandwidth measurements over idle windows of at least 65 seconds")


if __name__ == "__main__":
    try:
        with open(os.environ["IRIS_STACK_GATE_RECEIPT"]) as stream:
            check(json.load(stream), sys.argv[1])
    except (OSError, ValueError, KeyError, TypeError, IndexError, AttributeError) as error:
        sys.exit(f"Mesh release gate required: {error}. Set IRIS_STACK_GATE_RECEIPT to the pinned Iris Stack gate receipt for this exact public candidate.")
