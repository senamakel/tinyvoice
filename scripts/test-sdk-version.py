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

    def test_parent_gitlink_names_exact_published_source_without_vendor_manifest(self):
        indexed = subprocess.run(
            ["git", "ls-files", "--stage", "--", "vendor/tinybus"],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        indexed_commit = re.search(r"^160000 ([0-9a-f]{40}) 0\t", indexed, re.M)
        if indexed_commit and indexed_commit[1] == TINYBUS_033:
            return

        # During source work the pointer may be changed before its gitlink is
        # staged. Inspect the parent tree diff, never the submodule manifest:
        # this keeps the guard runnable in a clean, nonrecursive checkout.
        diff = subprocess.run(
            ["git", "diff", "--raw", "HEAD", "--", "vendor/tinybus"],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        changed_commit = re.search(r"^:160000 160000 [0-9a-f]+ ([0-9a-f]{40}) ", diff, re.M)
        self.assertEqual(
            changed_commit[1] if changed_commit else indexed_commit[1] if indexed_commit else None,
            TINYBUS_033,
        )

    def test_release_callers_use_reviewed_canonical_workflow_commit(self):
        release = (ROOT / ".github/workflows/release.yml").read_text()
        callers = re.findall(r"uses: tinyhumansai/\.github/.github/workflows/[^\s]+@(\w+)", release)
        self.assertEqual(callers, [MODULE_RELEASE_WORKFLOW, MODULE_RELEASE_WORKFLOW])


if __name__ == "__main__":
    unittest.main()
