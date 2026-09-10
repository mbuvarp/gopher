"""Run with python3 -m unittest discover -s scripts -p 'test_*.py'."""

import os
from pathlib import Path
import plistlib
import tempfile
import unittest
from unittest.mock import patch

import bundle


class BundleTests(unittest.TestCase):
    def test_build_versions_increase_across_semantic_version_boundaries(self):
        versions = ["0.1.0", "0.1.1", "0.1.99", "0.2.0", "0.99.99", "1.0.0", "2.0.0"]
        builds = [tuple(map(int, bundle.bundle_version(v).split("."))) for v in versions]
        self.assertGreater(builds[0], (1, 0, 0))  # Installed development builds used "1".
        self.assertTrue(all(a < b for a, b in zip(builds, builds[1:])))
        for invalid in ("1", "1.2", "01.2.3", "1.2.3-beta.1", "1.2.3+build", "1.100.0", "9999.0.0"):
            with self.subTest(version=invalid), self.assertRaises(ValueError):
                bundle.bundle_version(invalid)

    def test_generated_metadata_uses_package_version_and_keeps_app_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "Info.plist"
            bundle.write_info(destination, "0.2.3")
            with destination.open("rb") as source:
                info = plistlib.load(source)
            self.assertEqual(info["CFBundleShortVersionString"], "0.2.3")
            self.assertEqual(info["CFBundleVersion"], "1.2.3")
            self.assertEqual(info["CFBundleIdentifier"], "dev.mbuvarp.gopher")
            self.assertEqual(info["LSMinimumSystemVersion"], "13.0")
            self.assertTrue(info["LSUIElement"])

    def test_release_never_silently_falls_back_to_ad_hoc_signing(self):
        with patch.dict(os.environ, {"GOPHER_SIGNING_IDENTITY": ""}), patch.object(bundle, "output", return_value="0 valid identities found"):
            with self.assertRaisesRegex(ValueError, "Release packaging requires"):
                bundle.signing_identity(True)
        with patch.dict(os.environ, {"GOPHER_SIGNING_IDENTITY": "-"}):
            with self.assertRaises(ValueError):
                bundle.signing_identity(True)
            self.assertEqual(bundle.signing_identity(False), "-")
        with patch.dict(os.environ, {"GOPHER_SIGNING_IDENTITY": "explicit-identity"}):
            self.assertEqual(bundle.signing_identity(True), "explicit-identity")

    def test_missing_development_identity_does_not_select_an_unrelated_certificate(self):
        with patch.dict(os.environ, {"GOPHER_SIGNING_IDENTITY": ""}), patch.object(
            bundle, "output", return_value='1) ' + 'A' * 40 + ' "Apple Distribution: Someone"'
        ):
            with self.assertRaises(ValueError):
                bundle.signing_identity(True)

    def test_replacing_bundle_removes_stale_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            stage, dist = root / "stage", root / "dist"
            for directory in (stage, dist):
                (directory / "Gopher.app").mkdir(parents=True)
            (dist / "Gopher.app/obsolete").write_text("old")
            (stage / "Gopher.app/current").write_text("new")
            bundle.publish([stage / "Gopher.app"], dist)
            self.assertEqual((dist / "Gopher.app/current").read_text(), "new")
            self.assertFalse((dist / "Gopher.app/obsolete").exists())

    def test_publication_failure_restores_prior_bundle_and_archive(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            stage, dist = root / "stage", root / "dist"
            for directory in (stage, dist):
                (directory / "Gopher.app").mkdir(parents=True)
                (directory / "Gopher.app/binary").write_text(directory.name)
                (directory / "archive.zip").write_text(directory.name)
            original_rename = Path.rename

            def fail_archive(source, target):
                if source == stage / "archive.zip":
                    raise OSError("simulated replacement failure")
                return original_rename(source, target)

            with patch.object(Path, "rename", fail_archive), self.assertRaises(OSError):
                bundle.publish([stage / "Gopher.app", stage / "archive.zip"], dist)
            self.assertEqual((dist / "Gopher.app/binary").read_text(), "dist")
            self.assertEqual((dist / "archive.zip").read_text(), "dist")


if __name__ == "__main__":
    unittest.main()
