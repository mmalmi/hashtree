import copy
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("mesh_guard", ROOT / "scripts/check-mesh-release.py")
guard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guard)
SHA = "a" * 40


def receipt(revision=SHA):
    return {"schema_version": 1, "status": "passed", "release_gate": True,
            "lab_revision": guard.LAB_REV, "lab_worktree_clean": True, "products": {
                "hashtree": {"source": "https://github.com/mmalmi/hashtree", "rev": revision, "sha256": "b" * 64},
                "chat": {"source": "https://github.com/irislib/iris-chat-rs", "rev": "2270f5778fecf1e2eea7d47a4c382aacad63d551", "sha256": "b" * 64},
                "drive": {"source": "htree://npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/iris-drive", "rev": "7cb74966ddaecf90fb91b8f36a44ecc4bbda7b02", "sha256": "b" * 64}},
            "metrics": {"idle": [{"seconds": 65, "cpu_required": True, "cpu_budget_percent": 5,
                        "wire_budget_bytes_per_second": 4096, "cpu_percent": [0, 0.5, 5],
                        "combined_bytes_per_second": 3000} for _ in range(2)]}}


class MeshReleaseTests(unittest.TestCase):
    def test_exact_candidate_and_configured_companions_pass(self):
        guard.check(receipt(), SHA)

    def test_stale_candidate_lab_and_companions_fail(self):
        for path in [("lab_revision",), ("products", "hashtree", "rev"),
                     ("products", "drive", "rev"), ("products", "chat", "rev"),
                     ("products", "hashtree", "source"), ("products", "chat", "sha256")]:
            candidate = receipt()
            value = candidate
            for key in path[:-1]:
                value = value[key]
            value[path[-1]] = "wrong"
            with self.subTest(path=path), self.assertRaises(ValueError):
                guard.check(candidate, SHA)

    def test_missing_relaxed_and_invalid_cpu_measurements_fail(self):
        for key, value in [("cpu_required", False), ("cpu_budget_percent", 100),
                           ("cpu_percent", [0, None, 0]), ("cpu_percent", [0, True, 0]),
                           ("cpu_percent", [0, float("nan"), 0]), ("cpu_percent", [0, 5.1, 0]),
                           ("combined_bytes_per_second", 4096)]:
            candidate = copy.deepcopy(receipt())
            candidate["metrics"]["idle"][0][key] = value
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                guard.check(candidate, SHA)

    def test_each_idle_window_must_cover_maintenance(self):
        for index in range(2):
            for seconds in [None, False, "65", 0, 15, 64.999, float("nan"), float("inf"), -float("inf")]:
                candidate = receipt()
                candidate["metrics"]["idle"][index]["seconds"] = seconds
                with self.subTest(index=index, seconds=seconds), self.assertRaises(ValueError):
                    guard.check(candidate, SHA)
            candidate = receipt()
            del candidate["metrics"]["idle"][index]["seconds"]
            with self.subTest(index=index, missing_seconds=True), self.assertRaises(ValueError):
                guard.check(candidate, SHA)

    def test_longer_idle_windows_pass(self):
        candidate = receipt()
        candidate["metrics"]["idle"][0]["seconds"] = 65.5
        candidate["metrics"]["idle"][1]["seconds"] = 120
        guard.check(candidate, SHA)

    def test_cli_missing_or_malformed_receipt_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "receipt.json"
            for value in [None, "not JSON", "null", "{}"]:
                if value is not None:
                    path.write_text(value)
                result = subprocess.run([sys.executable, str(ROOT / "scripts/check-mesh-release.py"), SHA],
                    env={**os.environ, "IRIS_STACK_GATE_RECEIPT": str(path)},
                    capture_output=True, text=True, timeout=5)
                self.assertNotEqual(result.returncode, 0)

    def test_hosted_release_requires_the_exact_candidate_gate(self):
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        self.assertIn(f"product-lab.yml@{guard.LAB_REV}", workflow)
        self.assertIn("htree_rev: ${{ needs.source-ci.outputs.sha }}", workflow)
        self.assertIn("chat_rev: 2270f5778fecf1e2eea7d47a4c382aacad63d551", workflow)
        self.assertIn("drive_rev: 7cb74966ddaecf90fb91b8f36a44ecc4bbda7b02", workflow)
        release = workflow.split("\n  release:\n", 1)[1]
        self.assertIn("needs.mesh-resource.result == 'success'", release)
        self.assertIn("      - mesh-resource\n", release.split("    runs-on:", 1)[0])


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--fixture":
        print(json.dumps(receipt(sys.argv[2])))
    else:
        unittest.main()
