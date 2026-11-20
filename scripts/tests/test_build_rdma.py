#!/usr/bin/env python3
"""Check the Cargo invocation used to build the native RDMA executable."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class RdmaBuildTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.cwd = Path(temp.name)
        cargo = self.cwd / "cargo"
        cargo.write_text("#!/bin/sh\nprintf '%s\\n' \"$@\"\nexit \"${CARGO_TEST_EXIT:-0}\"\n")
        cargo.chmod(0o755)
        self.env = dict(os.environ, PATH=str(self.cwd) + os.pathsep + os.environ["PATH"])
        self.env.pop("RUSTFS_RDMA_TOOLCHAIN", None)

    def run_build(self, *args):
        return subprocess.run(
            [str(ROOT / "scripts/build-rdma.sh"), *args],
            cwd=self.cwd, env=self.env, capture_output=True, text=True, check=False,
        )

    def test_builds_locked_native_package_and_forwards_arguments(self):
        result = self.run_build("--release", "--target-dir", "build dir")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [
            "build", "--locked", "--manifest-path", str(ROOT / "nixl/Cargo.toml"),
            "--release", "--target-dir", "build dir",
        ])

    def test_explicit_toolchain_and_cargo_exit_status(self):
        self.env.update(RUSTFS_RDMA_TOOLCHAIN="stable", CARGO_TEST_EXIT="17")
        result = self.run_build()
        self.assertEqual(result.returncode, 17, result.stderr)
        self.assertEqual(result.stdout.splitlines()[0], "+stable")


if __name__ == "__main__":
    unittest.main()
