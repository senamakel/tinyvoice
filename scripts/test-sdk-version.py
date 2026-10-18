"""Keep TinyVoice's typed client and module on the published TinyBus SDK."""
import re
import subprocess
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
TINYBUS_033 = "207a9a7df15e44fdba00a807d0f7eaf21904ba0e"
MODULE_RELEASE_WORKFLOW = "6ed83b04aab2d8977ec62e966cb914531e534fa4"


class SdkVersionTests(unittest.TestCase):
    def test_client_and_module_require_published_tinybus_033(self):
        client = (ROOT / "crates/tinyvoice-client/Cargo.toml").read_text()
        module = (ROOT / "crates/tinyvoice-module/Cargo.toml").read_text()

        self.assertRegex(client, r'tinybus\s*=\s*\{[^}]*version\s*=\s*">=0\.3\.3, <0\.4"')
        self.assertRegex(module, r'tinybus\s*=\s*\{[^}]*version\s*=\s*">=0\.3\.3, <0\.4"')
        self.assertRegex(module, r'tinybus-module\s*=\s*">=0\.3\.3, <0\.4"')

    def test_client_fixtures_advertise_tinyvoice_module_version_033(self):
        for relative in (
            "crates/tinyvoice-client/src/lib_tests.rs",
            "crates/tinyvoice-client/src/lib_ownership_tests.rs",
        ):
            with self.subTest(fixture=relative):
                source = (ROOT / relative).read_text()
                self.assertIn('version: "0.3.3",', source)

    def test_parent_tree_gitlink_names_exact_published_source_without_vendor_manifest(self):
        tree = subprocess.run(
            ["git", "ls-tree", "HEAD", "--", "vendor/tinybus"],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        pinned_commit = re.search(
            r"^160000 commit ([0-9a-f]{40})\tvendor/tinybus$", tree, re.M
        )
        self.assertIsNotNone(pinned_commit, "parent tree must contain the TinyBus gitlink")
        self.assertEqual(pinned_commit[1], TINYBUS_033)

    def test_release_callers_use_reviewed_canonical_workflow_commit(self):
        release = (ROOT / ".github/workflows/release.yml").read_text()
        callers = re.findall(r"uses: tinyhumansai/\.github/.github/workflows/[^\s]+@(\w+)", release)
        self.assertEqual(callers, [MODULE_RELEASE_WORKFLOW, MODULE_RELEASE_WORKFLOW])


if __name__ == "__main__":
    unittest.main()
