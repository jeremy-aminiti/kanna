import importlib.util
import os
import subprocess
import sys
import tempfile
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


class SysrootOverlayTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if "TEST_SRCDIR" not in os.environ:
            raise unittest.SkipTest("overlay integration fixtures run under Bazel")
        runfiles = Path(os.environ["TEST_SRCDIR"])
        matches = list(runfiles.rglob("sysroot_overlay_test_tool"))
        if len(matches) != 1:
            raise AssertionError(f"expected one overlay test tool, found {matches}")
        cls.overlay = matches[0]

    def setUp(self):
        self.temporary_directory = tempfile.TemporaryDirectory(
            dir=os.environ.get("TEST_TMPDIR")
        )
        self.root = Path(self.temporary_directory.name)
        self.sysroot = self.root / "sysroot"
        self.sysroot.mkdir()
        self.report = self.sysroot / ".kanna-sysroot-overlay-report"
        self.report.write_text("", encoding="utf-8")

    def tearDown(self):
        self.temporary_directory.cleanup()

    def overlay_package(self, name, entries):
        staging = self.root / name
        staging.mkdir()
        for path, kind, value in entries:
            destination = staging / path
            destination.parent.mkdir(parents=True, exist_ok=True)
            if kind == "file":
                destination.write_text(value, encoding="utf-8")
            elif kind == "symlink":
                destination.symlink_to(value)
            else:
                raise AssertionError(f"unknown fixture kind {kind}")
        return subprocess.run(
            [self.overlay, staging, self.sysroot, name, self.report],
            check=False,
            capture_output=True,
            text=True,
        )

    def test_absolute_symlink_is_remapped_inside_sysroot(self):
        result = self.overlay_package(
            "absolute",
            [("usr/lib/python/sitecustomize.py", "symlink", "/etc/python/sitecustomize.py")],
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        link = self.sysroot / "usr/lib/python/sitecustomize.py"
        self.assertEqual(os.readlink(link), "../../../etc/python/sitecustomize.py")

    def test_relative_symlink_escaping_sysroot_is_rejected(self):
        result = self.overlay_package(
            "escape",
            [("usr/lib/escape", "symlink", "../../../outside")],
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("relative symlink escapes sysroot", result.stderr)
        self.assertFalse((self.sysroot / "usr/lib/escape").exists())

    def test_in_root_relative_symlink_is_preserved_canonically(self):
        result = self.overlay_package(
            "inside",
            [("usr/lib/link", "symlink", "../share/data")],
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(os.readlink(self.sysroot / "usr/lib/link"), "../share/data")

    def test_identical_duplicate_file_is_accepted_deterministically(self):
        first = self.overlay_package("first", [("usr/include/shared.h", "file", "same\n")])
        second = self.overlay_package("second", [("usr/include/shared.h", "file", "same\n")])
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertEqual((self.sysroot / "usr/include/shared.h").read_text(), "same\n")
        self.assertIn("identical-file second usr/include/shared.h", self.report.read_text())

    def test_conflicting_duplicate_file_is_rejected(self):
        first = self.overlay_package("first", [("usr/include/shared.h", "file", "one\n")])
        second = self.overlay_package("second", [("usr/include/shared.h", "file", "two\n")])
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertNotEqual(second.returncode, 0)
        self.assertIn("conflicting duplicate path usr/include/shared.h", second.stderr)
        self.assertEqual((self.sysroot / "usr/include/shared.h").read_text(), "one\n")

    def test_bazel_repository_output_records_real_archive_normalization(self):
        reports = list(Path(os.environ["TEST_SRCDIR"]).rglob(".kanna-sysroot-overlay-report"))
        self.assertEqual(len(reports), 1, reports)
        contents = reports[0].read_text(encoding="utf-8")
        self.assertIn(
            "usr/lib/python3.12/sitecustomize.py /etc/python3.12/sitecustomize.py "
            "-> ../../../etc/python3.12/sitecustomize.py",
            contents,
        )


if __name__ == "__main__":
    unittest.main()
