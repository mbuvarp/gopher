"""Exercise install.sh with fake GitHub/downloads and real ZIP extraction.

The fake app records invocation; these tests never install or launch Gopher.
"""
import hashlib
from pathlib import Path
import plistlib
import shlex
import subprocess
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parent.parent


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="gopher installer test ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.called = self.root / "helper-called"
        self.archive = self.root / "release.zip"
        self.checksum = self.root / "release.sha256"
        self.tag = "v0.1.0"
        self.sign_ok = True
        self.members = {
            "Gopher.app/Contents/Info.plist": plistlib.dumps({"CFBundleShortVersionString": "0.1.0", "GopherInstallerProtocol": 1}),
            "Gopher.app/Contents/MacOS/gopher": f"#!/bin/bash\nprintf '%s\\n' \"$@\" > {shlex.quote(str(self.called))}\n".encode(),
        }

    def executable(self, name, body):
        path = self.root / name
        path.write_text("#!/bin/bash\nset -eu\n" + body)
        path.chmod(0o755)
        return shlex.quote(str(path))

    def invoke(self, args=(), corrupt=False):
        with zipfile.ZipFile(self.archive, "w") as archive:
            for name, contents in self.members.items():
                info = zipfile.ZipInfo(name)
                info.create_system = 3
                info.external_attr = 0o100755 << 16
                archive.writestr(info, contents)
        digest = "0" * 64 if corrupt else hashlib.sha256(self.archive.read_bytes()).hexdigest()
        self.checksum.write_text(f"{digest}  Gopher-0.1.0-macos-arm64.zip\n")
        gh = self.executable("gh", f"if [ \"$1\" = auth ]; then exit 0; fi\nprintf '%s\\n' {shlex.quote(self.tag)}\n")
        curl = self.executable("curl", f"""
out=''
source={shlex.quote(str(self.archive))}
for arg in "$@"; do
    case "$arg" in
        *sha256) source={shlex.quote(str(self.checksum))} ;;
    esac
done
while [ "$#" -gt 0 ]; do
    if [ "$1" = --output ]; then out=$2; shift; fi
    shift
done
/bin/cp "$source" "$out"
""")
        script = (ROOT / "scripts/install.sh").read_text()
        script = script.replace('gh=$(command -v gh || true)', f'gh={gh}')
        script = script.replace('/usr/bin/curl', curl)
        script = script.replace('/usr/bin/codesign', self.executable("codesign", f"""
while [ "$#" -gt 0 ]; do
    if [ "$1" = -R ]; then
        case "$2" in =anchor*) ;; *) exit 1 ;; esac
    fi
    shift
done
exit {0 if self.sign_ok else 1}
"""))
        script = script.replace('/usr/bin/file', self.executable("file", "echo 'Mach-O 64-bit executable arm64'\n"))
        path = self.root / "install.sh"
        path.write_text(script)
        return subprocess.run(["/bin/bash", str(path), *args], text=True, capture_output=True)

    def test_verified_archive_invokes_helper_and_forwards_no_launch(self):
        result = self.invoke(["--no-launch"])
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.called.read_text().splitlines(), ["install", "--no-launch"])
        self.assertNotIn("unbound variable", result.stderr)

    def test_checksum_failure_never_invokes_the_helper(self):
        result = self.invoke(corrupt=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertFalse(self.called.exists())

    def test_wrong_signature_never_invokes_the_helper(self):
        self.sign_ok = False
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("signature verification failed", result.stderr)
        self.assertFalse(self.called.exists())

    def test_traversal_and_unexpected_files_are_rejected(self):
        for member in ("../escape", "Gopher.app/../../escape", "another.app/executable"):
            with self.subTest(member=member):
                self.members[member] = b"bad"
                result = self.invoke()
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(self.called.exists())
                del self.members[member]

    def test_release_tag_is_validated_before_becoming_a_download_url(self):
        self.tag = "v0.1.0/../../anything"
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsupported version tag", result.stderr)
        self.assertFalse(self.called.exists())

    def test_mismatched_bundle_version_is_rejected(self):
        self.members["Gopher.app/Contents/Info.plist"] = plistlib.dumps({"CFBundleShortVersionString": "0.2.0", "GopherInstallerProtocol": 1})
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("version does not match", result.stderr)
        self.assertFalse(self.called.exists())

    def test_tar_does_not_follow_an_archive_symlink_outside_staging(self):
        outside = self.root / "outside"
        outside.mkdir()
        original = zipfile.ZipFile.writestr

        def symlink(archive, info, contents, *args, **kwargs):
            if info.filename == "Gopher.app/link":
                info.external_attr = 0o120777 << 16
            return original(archive, info, contents, *args, **kwargs)

        from unittest.mock import patch
        self.members["Gopher.app/link"] = str(outside).encode()
        self.members["Gopher.app/link/escape"] = b"must not be written"
        with patch.object(zipfile.ZipFile, "writestr", symlink):
            result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((outside / "escape").exists())
        self.assertFalse(self.called.exists())
