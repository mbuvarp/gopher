"""Import CI-only signing secrets into disposable runner storage, without logging them."""
import base64
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import sys


def command(*args):
    result = subprocess.run(args, capture_output=True, text=True)
    if result.returncode:
        # Arguments include passwords; never include command or stderr in errors.
        raise ValueError(f"Signing setup failed running {Path(args[0]).name}")
    return result.stdout


def paths():
    root = Path(os.environ["RUNNER_TEMP"]) / "gopher-signing"
    return root, root / "signing.keychain-db", root / "certificate.p12", root / "sparkle.key"


def cleanup():
    root, keychain, certificate, key = paths()
    try:
        if keychain.exists():
            command("security", "delete-keychain", str(keychain))
    finally:
        for path in (certificate, key):
            path.unlink(missing_ok=True)
        if root.exists() and not any(root.iterdir()):
            root.rmdir()


def setup():
    root, keychain, certificate, key = paths()
    root.mkdir(mode=0o700)
    try:
        certificate.write_bytes(base64.b64decode(os.environ["GOPHER_APPLE_CERTIFICATE_P12"], validate=True))
        certificate.chmod(0o600)
        key.write_text(os.environ["GOPHER_SPARKLE_PRIVATE_KEY"].strip())
        key.chmod(0o600)
        if len(base64.b64decode(key.read_text(), validate=True)) not in (32, 96):
            raise ValueError("Invalid Sparkle private key")
        password = secrets.token_urlsafe(32)
        command("security", "create-keychain", "-p", password, str(keychain))
        command("security", "set-keychain-settings", "-lut", "3600", str(keychain))
        command("security", "unlock-keychain", "-p", password, str(keychain))
        command("security", "import", str(certificate), "-k", str(keychain), "-P",
                os.environ["GOPHER_APPLE_CERTIFICATE_PASSWORD"], "-T", "/usr/bin/codesign")
        command("security", "set-key-partition-list", "-S", "apple-tool:,apple:", "-s", "-k", password, str(keychain))
        existing = command("security", "list-keychains", "-d", "user")
        command("security", "list-keychains", "-d", "user", "-s", str(keychain),
                *[json.loads(line.strip()) for line in existing.splitlines() if line.strip()])
        identities = command("security", "find-identity", "-v", "-p", "codesigning", str(keychain))
        matches = re.findall(r'\b([A-F0-9]{40}) "Apple Development:', identities)
        if len(matches) != 1:
            raise ValueError("Expected exactly one Apple Development signing identity")
        with open(os.environ["GITHUB_ENV"], "a") as env:
            env.write(f"GOPHER_SIGNING_IDENTITY={matches[0]}\nGOPHER_UPDATE_KEY_FILE={key}\n")
    finally:
        certificate.unlink(missing_ok=True)


if __name__ == "__main__":
    try:
        if sys.argv[1:] == ["cleanup"]:
            cleanup()
        elif sys.argv[1:] == ["setup"]:
            setup()
        else:
            raise ValueError("Expected setup or cleanup")
    except (ValueError, OSError, KeyError):
        sys.exit("Signing setup/cleanup failed. Check the release environment secrets and signing identity.")
