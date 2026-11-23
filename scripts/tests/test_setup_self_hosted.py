#!/usr/bin/env python3
"""Exercise the real composite action with isolated package-manager doubles."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
ACTION = ROOT / ".github/actions/setup-self-hosted/action.yml"


class HostSetupTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.cwd = Path(temp.name)
        self.log = self.cwd / "apt.jsonl"
        self.release = self.cwd / "os-release"
        self.release.write_text("ID=ubuntu\nVERSION_CODENAME=noble\n")
        keyring = self.cwd / "archive-keyring.gpg"
        keyring.write_text("fixture signing keyring")
        body = ACTION.read_text().split("      run: |\n", 1)[1]
        body = "\n".join(line[8:] if line.startswith("        ") else line for line in body.splitlines())
        body = body.replace(". /etc/os-release", f'. "{self.release}"')
        for distro in ("ubuntu", "debian"):
            body = body.replace(f"/usr/share/keyrings/{distro}-archive-keyring.gpg", str(keyring))
        self.script = self.cwd / "setup.sh"
        self.script.write_text(body)
        self.env = dict(os.environ, PATH=str(self.cwd) + os.pathsep + os.environ["PATH"],
                        APT_TEST_LOG=str(self.log), PROFILE="curl", INSTALLED_PACKAGES="all", APT_TEST_ARCH="amd64")
        self.stub("sudo", '#!/bin/bash\nif [[ "$1" == DEBIAN_FRONTEND=* ]]; then shift; fi\nexec "$@"\n')
        self.stub("dpkg", '#!/bin/sh\nprintf "%s\\n" "$APT_TEST_ARCH"\n')
        self.stub("dpkg-query", '''#!/usr/bin/env python3
import os, sys
installed = os.environ["INSTALLED_PACKAGES"].split()
if "all" in installed or sys.argv[-1] in installed:
    print("install ok installed", end="")
else:
    sys.exit(1)
''')
        self.stub("apt-get", '''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
args = sys.argv[1:]
source = next(value.split("=", 1)[1] for value in args if value.startswith("Dir::Etc::sourcelist="))
with open(os.environ["APT_TEST_LOG"], "a") as log:
    log.write(json.dumps({"args": args, "sources": Path(source).read_text()}) + "\\n")
operation = "install" if "install" in args else "update"
code = os.environ.get("APT_TEST_EXIT", "0") if os.environ.get("APT_TEST_FAIL_OPERATION", operation) == operation else "0"
sys.exit(int(code))
''')

    def stub(self, name, content):
        executable = self.cwd / name
        executable.write_text(content)
        executable.chmod(0o755)

    def run_setup(self):
        return subprocess.run(["bash", str(self.script)], env=self.env, capture_output=True, text=True, check=False)

    def apt_calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def test_installed_tools_do_not_contact_any_apt_repository(self):
        result = self.run_setup()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.apt_calls(), [])

    def test_missing_build_tools_use_the_same_isolated_signed_sources_for_update_and_install(self):
        self.env.update(PROFILE="rust-build", INSTALLED_PACKAGES="git curl ca-certificates")
        result = self.run_setup()
        self.assertEqual(result.returncode, 0, result.stderr)
        update, install = self.apt_calls()
        self.assertIn("update", update["args"])
        self.assertIn("install", install["args"])
        options = lambda call: [value for value in call["args"] if value.startswith("Dir::")]
        self.assertEqual(options(update), options(install))
        self.assertIn("Dir::Etc::sourceparts=-", update["args"])
        self.assertIn("signed-by=", update["sources"])
        self.assertIn("http://archive.ubuntu.com/ubuntu noble main universe", update["sources"])
        self.assertIn("http://security.ubuntu.com/ubuntu noble-security", update["sources"])
        self.assertNotIn("git", install["args"])
        self.assertIn("--no-upgrade", install["args"])

    def test_arm64_uses_the_official_ubuntu_ports_archive(self):
        self.env.update(INSTALLED_PACKAGES="", APT_TEST_ARCH="arm64")
        result = self.run_setup()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("http://ports.ubuntu.com/ubuntu-ports", self.apt_calls()[0]["sources"])

    def test_debian_uses_its_distribution_keyring_and_security_archive(self):
        self.release.write_text("ID=debian\nVERSION_CODENAME=trixie\n")
        self.env["INSTALLED_PACKAGES"] = ""
        result = self.run_setup()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("http://security.debian.org/debian-security trixie-security main", self.apt_calls()[0]["sources"])

    def test_repository_failure_stops_before_installing(self):
        self.env.update(INSTALLED_PACKAGES="", APT_TEST_EXIT="100")
        result = self.run_setup()
        self.assertEqual(result.returncode, 100, result.stderr)
        self.assertEqual(len(self.apt_calls()), 1)

    def test_unknown_profile_is_rejected_without_package_operations(self):
        self.env["PROFILE"] = "unexpected"
        result = self.run_setup()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unknown setup profile", result.stderr)
        self.assertEqual(self.apt_calls(), [])

    def test_install_failure_is_not_swallowed(self):
        self.env.update(INSTALLED_PACKAGES="", APT_TEST_EXIT="100", APT_TEST_FAIL_OPERATION="install")
        result = self.run_setup()
        self.assertEqual(result.returncode, 100, result.stderr)
        self.assertEqual(len(self.apt_calls()), 2)

    def test_github_cli_adds_its_signed_source_only_to_the_temporary_context(self):
        self.env["PROFILE"] = "github-cli"
        self.stub("curl", '#!/bin/sh\nprintf "fixture keyring"\n')
        self.script.write_text('''command() {
  if [[ "$1" == "-v" && "$2" == "gh" ]]; then return 1; fi
  builtin command "$@"
}
''' + self.script.read_text())
        result = self.run_setup()
        self.assertEqual(result.returncode, 0, result.stderr)
        update, install = self.apt_calls()
        self.assertIn("https://cli.github.com/packages stable main", update["sources"])
        self.assertIn("Dir::Etc::sourceparts=-", install["args"])
        self.assertEqual(install["args"][-1], "gh")


if __name__ == "__main__":
    unittest.main()
