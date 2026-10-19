#!/usr/bin/env python3
"""Exercise the opt-in build boundary without loading a private dependency."""

from pathlib import Path
import os
import shutil
import subprocess
import tempfile
import unittest


class RdmaBuildTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "scripts").mkdir()
        self.script = self.root / "scripts/build-rdma.sh"
        shutil.copyfile(Path(__file__).parents[1] / "build-rdma.sh", self.script)
        self.env = dict(os.environ)
        self.env.pop("RUSTFS_RDMA_TOOLCHAIN", None)
        self.env.pop("CARGO_BUILD_JOBS", None)
        fake_bin = self.root / "bin"
        fake_bin.mkdir()
        cargo = fake_bin / "cargo"
        cargo.write_text("#!/bin/sh\n[ \"${RUSTFS_PRIVATE_BUILD_PROBE:-}\" = enabled ] || exit 7\nprintf '%s\\n' \"$@\"\n")
        cargo.chmod(0o755)
        self.env["PATH"] = str(fake_bin) + os.pathsep + self.env["PATH"]

    def run_build(self, *args):
        return subprocess.run(["bash", str(self.script), *args], env=self.env,
                              capture_output=True, text=True, check=False)

    def prepare_private(self):
        private = self.root / "vendor/rustfs-ucx"
        private.mkdir(parents=True)
        (private / "Cargo.toml").write_text('[package]\nname = "rustfs-ucx"\n')
        entry = private / "integrations/cli/src/main.rs"
        entry.parent.mkdir(parents=True)
        entry.write_text("// Fixture: only the build preflight reads this path.\n")
        (entry.parent / "lib.rs").touch()
        (private / "scripts").mkdir()
        (private / "scripts/build-env.sh").write_text("export RUSTFS_PRIVATE_BUILD_PROBE=enabled\n")

    def test_missing_private_source_fails_before_cargo(self):
        result = self.run_build()
        self.assertEqual(result.returncode, 2)
        self.assertIn("private submodule", result.stderr)
        self.assertEqual(result.stdout, "")

    def test_partial_checkout_is_rejected(self):
        private = self.root / "vendor/rustfs-ucx"
        private.mkdir(parents=True)
        (private / "Cargo.toml").write_text('[package]\nname = "rustfs-ucx"\n')
        result = self.run_build()
        self.assertEqual(result.returncode, 2)
        self.assertIn("private submodule", result.stderr)
        self.assertEqual(result.stdout, "")

    def test_entry_without_manifest_is_rejected(self):
        self.prepare_private()
        (self.root / "vendor/rustfs-ucx/Cargo.toml").unlink()
        result = self.run_build()
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stdout, "")

    def test_missing_private_build_environment_is_rejected(self):
        self.prepare_private()
        (self.root / "vendor/rustfs-ucx/scripts/build-env.sh").unlink()
        result = self.run_build()
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stdout, "")

    def test_opt_in_preserves_arguments_and_uses_separate_manifest(self):
        self.prepare_private()
        result = self.run_build("--release", "--offline")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), ["+1.98.1", "build", "--manifest-path",
            str(self.root / "rdma/Cargo.toml"), "--features", "rdma", "-j", "2",
            "--release", "--offline"])

    def test_toolchain_and_parallelism_are_explicit(self):
        self.prepare_private()
        self.env["RUSTFS_RDMA_TOOLCHAIN"] = "stable"
        self.env["CARGO_BUILD_JOBS"] = "3"
        result = self.run_build()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines()[0], "+stable")
        self.assertEqual(result.stdout.splitlines()[-2:], ["-j", "3"])


if __name__ == "__main__":
    unittest.main()
