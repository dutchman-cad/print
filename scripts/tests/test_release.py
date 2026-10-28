"""Synthetic changelog and archive checks; no builds or network access."""

import hashlib
import importlib.util
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import changelog

spec = importlib.util.spec_from_file_location("package_release", SCRIPTS / "package-release.py")
packaging = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packaging)


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        # Keep synthetic files in the checkout, including on shared machines.
        self.folder = tempfile.TemporaryDirectory(dir=SCRIPTS.parent)
        self.addCleanup(self.folder.cleanup)
        self.root = Path(self.folder.name)
        (self.root / "CHANGELOG.md").write_text("# Changelog\n\n## Unreleased\n")
        (self.root / "changelog.d").mkdir()
        (self.root / "changelog.d/a.md").write_text("### Docs\n\n- First.\n  Continued.\n")
        (self.root / "changelog.d/b.md").write_text("### Docs\n\n- Second.\n")
        (self.root / "changelog.d/README.md").write_text("Keep me.\n")

    def test_release_is_read_only_and_ordered(self):
        before = {p: p.read_bytes() for p in self.root.rglob("*.md")}
        section = changelog.release("0.1.0", "2026-10-28", self.root)
        self.assertEqual(section, "## 0.1.0 (2026-10-28)\n\n### Docs\n\n- First.\n  Continued.\n- Second.\n")
        self.assertEqual(before, {p: p.read_bytes() for p in self.root.rglob("*.md")})

    def test_explicit_write_and_existing_section(self):
        section = changelog.release("0.1.0", "2026-10-28", self.root, write=True)
        self.assertEqual(changelog.release("0.1.0", root=self.root), section)
        self.assertEqual(list((self.root / "changelog.d").iterdir()),
                         [self.root / "changelog.d/README.md"])
        with self.assertRaises(ValueError):
            changelog.release("0.1.0", root=self.root, write=True)

    def test_existing_section_excludes_other_versions_and_new_fragments(self):
        (self.root / "CHANGELOG.md").write_text(
            "# Changelog\n\n## Unreleased\n\n## 0.1.0 (2026-10-28)\n\n### Docs\n\n- Shipped.\n\n## 0.0.9\n\n- Older.\n")
        self.assertEqual(changelog.release("0.1.0", root=self.root),
                         "## 0.1.0 (2026-10-28)\n\n### Docs\n\n- Shipped.\n")

    def test_adjacent_version_headings(self):
        (self.root / "CHANGELOG.md").write_text("# Changelog\n\n## 0.1.0\n## 0.0.9\n")
        self.assertEqual(changelog.release("0.1.0", root=self.root), "## 0.1.0\n")

    def test_bad_version_date_fragment_and_empty_input(self):
        for version in ("v0.1.0", "../0.1.0", "0.1", "0.1.0\n"):
            with self.assertRaises(ValueError):
                changelog.release(version, root=self.root)
        with self.assertRaises(ValueError):
            changelog.release("0.1.0", "2026-02-30", self.root)
        (self.root / "changelog.d/a.md").write_text("### Unknown\n\n- Bad.\n")
        with self.assertRaises(ValueError):
            changelog.release("0.1.0", root=self.root)
        for path in (self.root / "changelog.d").glob("[ab].md"):
            path.unlink()
        with self.assertRaises(ValueError):
            changelog.release("0.1.0", root=self.root)

    def test_cli_prints_current_fragments_without_changes(self):
        root = SCRIPTS.parent
        paths = [root / "CHANGELOG.md", *sorted((root / "changelog.d").glob("*.md"))]
        before = {p: p.read_bytes() for p in paths}
        result = subprocess.run([sys.executable, str(SCRIPTS / "changelog.py"),
                                 "release", "0.1.0", "--date", "2026-10-28"],
                                capture_output=True, text=True, check=True)
        self.assertTrue(result.stdout.startswith("## 0.1.0"))
        self.assertIn("### Docs", result.stdout)
        self.assertEqual(before, {p: p.read_bytes() for p in paths})

    def test_all_target_archives_and_checksums(self):
        for item in ("README.md", "LICENSE", "NOTICE", "release-notes.md"):
            (self.root / item).write_text(f"Synthetic {item}\n")
        for target in packaging.TARGETS:
            with self.subTest(target=target):
                binary = "dutchman-print" + (".exe" if "windows" in target else "")
                path = self.root / "target" / target / "release" / binary
                path.parent.mkdir(parents=True)
                path.write_bytes(b"synthetic binary\n")
                path.chmod(0o755)
                archive = packaging.package(target, "0.1.0", self.root)
                prefix = f"dutchman-print-0.1.0-{target}/"
                expected = {prefix + p for p in (binary, "README.md", "LICENSE", "NOTICE", "CHANGELOG.md")}
                if archive.suffix == ".zip":
                    with zipfile.ZipFile(archive) as contents:
                        self.assertEqual(set(contents.namelist()), expected)
                        notes = contents.read(prefix + "CHANGELOG.md")
                else:
                    with tarfile.open(archive) as contents:
                        self.assertEqual(set(contents.getnames()), expected)
                        self.assertEqual(contents.getmember(prefix + binary).mode & 0o111, 0o111)
                        notes = contents.extractfile(prefix + "CHANGELOG.md").read()
                self.assertEqual(notes, (self.root / "release-notes.md").read_bytes())
                digest = hashlib.sha256(archive.read_bytes()).hexdigest()
                self.assertEqual(archive.with_name(archive.name + ".sha256").read_text(),
                                 f"{digest}  {archive.name}\n")

    def test_package_refuses_missing_inputs_and_invalid_version(self):
        with self.assertRaises(ValueError):
            packaging.package(packaging.TARGETS[0], "0.1.0", self.root)
        with self.assertRaises(ValueError):
            packaging.package(packaging.TARGETS[0], "../oops", self.root)


if __name__ == "__main__":
    unittest.main()
