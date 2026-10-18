"""Keep TinyVoice's typed client and module on the published TinyBus SDK."""
import re
import subprocess
import tomllib
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
TINYBUS_033 = "207a9a7df15e44fdba00a807d0f7eaf21904ba0e"
TINYBUS_GIT = "https://github.com/tinyhumansai/tinybus"
TINYBUS_REQUIREMENT = ">=0.3.3, <0.4.0"
ORDINARY_MODULE_RELEASE_WORKFLOW = "1e28317c435a9c714002199e65d18a1067fa5777"
REVIEWED_EPIC_MODULE_RELEASE_WORKFLOW = "6ed83b04aab2d8977ec62e966cb914531e534fa4"
SDK_CRATE_PATHS = {
    "tinybus": "crates/tinybus",
    "tinybus-module": "crates/tinybus-module",
    "tinybus-macros": "crates/tinybus-macros",
}


class SdkVersionTests(unittest.TestCase):
    def test_client_and_module_require_published_tinybus_033(self):
        client = (ROOT / "crates/tinyvoice-client/Cargo.toml").read_text()
        module = (ROOT / "crates/tinyvoice-module/Cargo.toml").read_text()

        self.assertRegex(client, r'tinybus\s*=\s*\{[^}]*version\s*=\s*">=0\.3\.3, <0\.4\.0"')
        self.assertRegex(module, r'tinybus\s*=\s*\{[^}]*version\s*=\s*">=0\.3\.3, <0\.4\.0"')

    @staticmethod
    def manifest(relative):
        return tomllib.loads((ROOT / relative).read_text())

    def assert_git_sdk_dependency(self, relative, crate):
        dependencies = self.manifest(relative)["dependencies"]
        dependency = dependencies[crate]
        self.assertIsInstance(dependency, dict, f"{crate} must use a source-qualified dependency")
        self.assertEqual(dependency.get("git"), TINYBUS_GIT)
        self.assertEqual(dependency.get("rev"), TINYBUS_033)
        self.assertEqual(dependency.get("version"), TINYBUS_REQUIREMENT)
        self.assertNotIn("path", dependency, f"{crate} must not bypass source identity with a path")

    def assert_local_git_source_patch(self, relative, prefix):
        patch = self.manifest(relative).get("patch", {}).get(TINYBUS_GIT)
        self.assertIsNotNone(patch, f"{relative} must patch the canonical TinyBus Git source")
        self.assertEqual(
            patch,
            {crate: {"path": f"{prefix}/{path}"} for crate, path in SDK_CRATE_PATHS.items()},
        )

    def test_client_uses_patchable_canonical_sdk_git_source(self):
        self.assert_git_sdk_dependency("crates/tinyvoice-client/Cargo.toml", "tinybus")

    def test_excluded_module_uses_patchable_canonical_sdk_git_source(self):
        for crate in ("tinybus", "tinybus-module"):
            with self.subTest(crate=crate):
                self.assert_git_sdk_dependency("crates/tinyvoice-module/Cargo.toml", crate)

    def test_standalone_workspace_patches_all_sdk_crates_to_vendor(self):
        self.assert_local_git_source_patch("Cargo.toml", "vendor/tinybus")

    def test_excluded_module_workspace_patches_all_sdk_crates_to_vendor(self):
        self.assert_local_git_source_patch("crates/tinyvoice-module/Cargo.toml", "../../vendor/tinybus")
        crates_io = self.manifest("crates/tinyvoice-module/Cargo.toml").get("patch", {}).get("crates-io", {})
        for crate in SDK_CRATE_PATHS:
            self.assertNotIn(crate, crates_io, f"{crate} must resolve from the canonical Git source")

    def test_client_fixtures_advertise_tinyvoice_module_version_033(self):
        for relative in (
            "crates/tinyvoice-client/src/lib_tests.rs",
            "crates/tinyvoice-client/src/lib_ownership_tests.rs",
        ):
            with self.subTest(fixture=relative):
                source = (ROOT / relative).read_text()
                self.assertIn('version: "0.3.3",', source)

    def test_client_is_source_consumed_and_not_registry_published(self):
        client = self.manifest("crates/tinyvoice-client/Cargo.toml")
        self.assertIs(client["package"].get("publish"), False)

        readme = (ROOT / "crates/tinyvoice-client/README.md").read_text()
        self.assertIn("not published to crates.io", readme)
        self.assertIn("path dependency", readme)
        self.assertIn("Git checkout", readme)
        self.assertIn("registry package", readme)

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

    def test_release_callers_preserve_ordinary_and_reviewed_workflow_pins(self):
        release = (ROOT / ".github/workflows/release.yml").read_text()
        callers = re.findall(r"uses: tinyhumansai/\.github/.github/workflows/[^\s]+@(\w+)", release)
        self.assertEqual(
            callers,
            [ORDINARY_MODULE_RELEASE_WORKFLOW, REVIEWED_EPIC_MODULE_RELEASE_WORKFLOW],
        )


if __name__ == "__main__":
    unittest.main()
