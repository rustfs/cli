#!/usr/bin/env python3

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


SCRIPT_PATH = Path(__file__).parents[1] / "rustfs_compatibility.py"


def load_module():
    spec = importlib.util.spec_from_file_location("rustfs_compatibility", SCRIPT_PATH)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class CompatibilityManifestTests(unittest.TestCase):
    def setUp(self):
        self.module = load_module()
        self.manifest = {
            "schema_version": 1,
            "supported_floor": "1.0.0-beta.9",
            "target": "1.0.0-beta.10",
            "servers": [
                {
                    "version": "1.0.0-beta.9",
                    "image": "rustfs/rustfs:1.0.0-beta.9",
                },
                {
                    "version": "1.0.0-beta.10",
                    "image": "rustfs/rustfs:1.0.0-beta.10",
                },
            ],
            "capabilities": [
                {
                    "id": "s3-data-plane",
                    "surface": "s3",
                    "probe": "rc-integration",
                    "expectations": {
                        "1.0.0-beta.9": "supported",
                        "1.0.0-beta.10": "supported",
                    },
                },
                {
                    "id": "admin-v4-runtime-capabilities",
                    "surface": "admin-v4",
                    "probe": "signed-http",
                    "expectations": {
                        "1.0.0-beta.9": "version-dependent",
                        "1.0.0-beta.10": "supported",
                    },
                },
                {
                    "id": "listen-notification-stream",
                    "surface": "streaming",
                    "probe": "signed-http",
                    "expectations": {
                        "1.0.0-beta.9": "version-dependent",
                        "1.0.0-beta.10": "supported",
                    },
                },
                {
                    "id": "batch-jobs",
                    "surface": "admin-v3",
                    "probe": "signed-http",
                    "expectations": {
                        "1.0.0-beta.9": "version-dependent",
                        "1.0.0-beta.10": "not-implemented",
                    },
                }
            ],
        }

    def test_validate_accepts_pinned_floor_and_target(self):
        self.module.validate_manifest(self.manifest)

    def test_validate_rejects_latest_image(self):
        self.manifest["servers"][1]["image"] = "rustfs/rustfs:latest"

        with self.assertRaisesRegex(ValueError, "must be pinned"):
            self.module.validate_manifest(self.manifest)

    def test_validate_requires_beta10_negative_stub(self):
        batch = next(
            capability
            for capability in self.manifest["capabilities"]
            if capability["id"] == "batch-jobs"
        )
        batch["expectations"]["1.0.0-beta.10"] = "supported"

        with self.assertRaisesRegex(ValueError, "batch-jobs"):
            self.module.validate_manifest(self.manifest)

    def test_report_combines_expectations_and_probe_results(self):
        probes = {
            "batch-jobs": {
                "result": "passed",
                "detail": "HTTP 501 NotImplemented",
            }
        }

        report = self.module.build_report(
            self.manifest, "1.0.0-beta.10", probes, "sha256:test"
        )

        self.assertEqual(report["server"]["image"], "rustfs/rustfs:1.0.0-beta.10")
        self.assertEqual(report["server"]["image_id"], "sha256:test")
        batch = next(
            capability
            for capability in report["capabilities"]
            if capability["id"] == "batch-jobs"
        )
        self.assertEqual(batch["expected"], "not-implemented")
        self.assertEqual(batch["result"], "passed")

    def test_write_report_produces_valid_json(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "report.json"
            self.module.write_report(
                output,
                self.module.build_report(
                    self.manifest, "1.0.0-beta.9", {}, "sha256:test"
                ),
            )

            document = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(document["server"]["version"], "1.0.0-beta.9")

    def test_repository_validation_requires_every_matrix_image(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            workflow = root / ".github/workflows/integration.yml"
            compose = root / "docker/docker-compose.yml"
            integration_test = root / "crates/cli/tests/integration.rs"
            for path in (workflow, compose, integration_test):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("pinned\n", encoding="utf-8")
            compose.write_text("rustfs/rustfs:1.0.0-beta.10\n", encoding="utf-8")

            with self.assertRaisesRegex(ValueError, "matrix server image"):
                self.module.validate_repository(root, self.manifest)


if __name__ == "__main__":
    unittest.main()
