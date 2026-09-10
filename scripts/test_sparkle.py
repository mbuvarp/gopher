import base64
from pathlib import Path
import plistlib
import tempfile
import unittest
from unittest.mock import patch
import bundle
import sparkle


class SparkleTests(unittest.TestCase):
    def test_release_enables_verified_daily_updates_but_local_build_does_not(self):
        with tempfile.TemporaryDirectory() as temporary:
            info = Path(temporary) / "Info.plist"
            bundle.write_info(info, "0.2.0")
            self.assertNotIn("SUFeedURL", plistlib.loads(info.read_bytes()))
            bundle.write_info(info, "0.2.0", release=True)
            values = plistlib.loads(info.read_bytes())
            self.assertTrue(values["GopherUpdatesEnabled"])
            self.assertTrue(values["SURequireSignedFeed"])
            self.assertTrue(values["SUVerifyUpdateBeforeExtraction"])
            self.assertEqual(values["SUSignedFeedFailureExpirationInterval"], 0)
            self.assertEqual(values["SUScheduledCheckInterval"], 86400)
            self.assertFalse(values["SUEnableSystemProfiling"])
            self.assertEqual(len(base64.b64decode(values["SUPublicEDKey"])), 32)

    def test_corrupt_cached_sdk_is_never_extracted_or_executed(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            cache = root / "target/sparkle"
            cache.mkdir(parents=True)
            (cache / f"Sparkle-{sparkle.VERSION}.tar.xz").write_bytes(b"corrupt")
            with patch.object(sparkle, "ROOT", root), patch.object(sparkle.subprocess, "run") as run:
                with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                    sparkle.sdk()
                run.assert_not_called()

    def test_feed_requires_signed_matching_archive_and_version(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "Gopher-0.2.0-macos-arm64.zip"
            archive.write_bytes(b"archive")
            feed = root / "appcast.xml"
            signature = base64.b64encode(bytes(64)).decode()
            xml = f'''<rss xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle"><channel><item>
                <sparkle:version>1.2.0</sparkle:version><sparkle:shortVersionString>0.2.0</sparkle:shortVersionString>
                <enclosure url="https://github.com/mbuvarp/gopher/releases/download/v0.2.0/{archive.name}" length="7" sparkle:edSignature="{signature}"/>
                </item></channel></rss>'''
            feed.write_text(xml)
            self.assertEqual(sparkle.validate_feed(feed, archive, "0.2.0"), signature)
            for invalid in [xml.replace(signature, ""), xml.replace('length="7"', 'length="8"'),
                            xml.replace("v0.2.0/", "latest/"), xml.replace("1.2.0", "1.1.0")]:
                feed.write_text(invalid)
                with self.assertRaises(ValueError):
                    sparkle.validate_feed(feed, archive, "0.2.0")
