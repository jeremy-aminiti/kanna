import importlib.util
import sys
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).parents[1] / "resolve_sysroot.py"
SPEC = importlib.util.spec_from_file_location("resolve_sysroot", MODULE_PATH)
assert SPEC and SPEC.loader
resolver = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = resolver
SPEC.loader.exec_module(resolver)


def package(name, *, depends="", provides=(), architecture="amd64"):
    return resolver.Package(
        name=name,
        version="1",
        architecture=architecture,
        filename=f"pool/{name}.deb",
        sha256="a" * 64,
        size=1,
        depends=depends,
        pre_depends="",
        provides=tuple(provides),
    )


class ResolveSysrootTest(unittest.TestCase):
    def test_seeds_include_every_runtime_policy_package(self):
        policy = resolver.json.loads(
            (MODULE_PATH.parent / "runtime-policy.json").read_text(encoding="utf-8")
        )
        runtime_packages = {
            entry["package"] for entry in policy["allowedRuntimeLibraries"]
        }
        self.assertTrue(runtime_packages.issubset(set(resolver.SEEDS)))
        self.assertTrue(set(resolver.DEVELOPMENT_SEEDS).issubset(set(resolver.SEEDS)))

    def test_control_continuations(self):
        self.assertEqual(
            resolver.parse_control("Package: one\nDescription: first\n second\n\nPackage: two\n"),
            [
                {"Package": "one", "Description": "first\nsecond"},
                {"Package": "two"},
            ],
        )

    def test_relations_preserve_groups_and_filter_architectures(self):
        value = "one (>= 1) | two:any, three [amd64] | four [arm64], five:linux-any"
        self.assertEqual(
            resolver.relation_alternatives(value, "amd64"),
            [["one", "two"], ["three"], ["five"]],
        )

    def test_resolution_uses_first_available_alternative_and_unique_provider(self):
        original = resolver.SEEDS
        try:
            resolver.SEEDS = ("root",)
            resolved = resolver.resolve_packages(
                [
                    package("root", depends="missing | actual, virtual"),
                    package("actual"),
                    package("provider", provides=("virtual",)),
                ],
                "amd64",
            )
        finally:
            resolver.SEEDS = original
        self.assertEqual([item.name for item in resolved], ["actual", "provider", "root"])

    def test_resolution_rejects_ambiguous_virtual_provider(self):
        original = resolver.SEEDS
        try:
            resolver.SEEDS = ("root",)
            with self.assertRaisesRegex(ValueError, "no unambiguous candidate"):
                resolver.resolve_packages(
                    [
                        package("root", depends="virtual"),
                        package("one", provides=("virtual",)),
                        package("two", provides=("virtual",)),
                    ],
                    "amd64",
                )
        finally:
            resolver.SEEDS = original

    def test_target_architecture_wins_over_all(self):
        original = resolver.SEEDS
        try:
            resolver.SEEDS = ("root",)
            resolved = resolver.resolve_packages(
                [package("root", architecture="all"), package("root", architecture="amd64")],
                "amd64",
            )
        finally:
            resolver.SEEDS = original
        self.assertEqual(resolved[0].architecture, "amd64")

    def test_committed_locks_are_architecture_specific_and_hash_locked(self):
        for architecture in resolver.ARCHITECTURES:
            lock = resolver.json.loads(
                (MODULE_PATH.parent / f"sysroot-{architecture}.lock.json").read_text(encoding="utf-8")
            )
            self.assertEqual(lock["architecture"], architecture)
            self.assertEqual(lock["snapshot"], resolver.SNAPSHOT)
            self.assertEqual(lock["snapshotRoot"], resolver.SNAPSHOT_ROOT)
            self.assertEqual(lock["seeds"], sorted(set(resolver.SEEDS)))
            self.assertTrue(lock["indexes"])
            self.assertTrue(lock["packages"])
            for item in [*lock["indexes"], *lock["packages"]]:
                self.assertRegex(item["sha256"], r"^[0-9a-f]{64}$")
                self.assertGreater(item["size"], 0)

    def test_toolchain_source_uses_one_zig_and_never_host_paths(self):
        repo_root = MODULE_PATH.parents[2]
        module = (repo_root / "MODULE.bazel").read_text(encoding="utf-8")
        toolchain = (repo_root / "tools/bazel/linux_cc_toolchain_config.bzl").read_text(encoding="utf-8")
        build = (repo_root / "tools/bazel/BUILD.bazel").read_text(encoding="utf-8")
        self.assertEqual(module.count("zig.toolchain("), 1)
        self.assertIn("x86_64-linux-gnu.2.39", build)
        self.assertIn("aarch64-linux-gnu.2.39", build)
        self.assertNotIn("@rules_z//", build)
        for forbidden in ("/opt/homebrew", "/usr/bin", "PKG_CONFIG_PATH"):
            self.assertNotIn(forbidden, toolchain)


if __name__ == "__main__":
    unittest.main()
