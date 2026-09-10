"""Pinned Sparkle SDK, bundle integration, and signed appcast generation."""
import hashlib
import base64
import xml.etree.ElementTree as ET
from pathlib import Path
import shutil
import subprocess
import tempfile
import urllib.request

ROOT = Path(__file__).resolve().parent.parent
VERSION = "2.9.6"
SHA256 = "52bf9e88cdd972fc0c81501377a880e90d47031bd8ca5462488f843e2609e192"
PUBLIC_KEY = "sm+JvRDBL7eG2rk4+O/lIkcMW2/kmTBBGXt6NduRM6w="
KEY_ACCOUNT = "dev.mbuvarp.gopher"
FEED_URL = "https://github.com/mbuvarp/gopher/releases/latest/download/appcast.xml"


def sdk():
    """Verify the SDK archive on every use, including an already cached download."""
    cache = ROOT / "target/sparkle"
    cache.mkdir(parents=True, exist_ok=True)
    archive = cache / f"Sparkle-{VERSION}.tar.xz"
    if not archive.exists():
        with tempfile.NamedTemporaryFile(dir=cache) as temporary:
            with urllib.request.urlopen(
                f"https://github.com/sparkle-project/Sparkle/releases/download/{VERSION}/{archive.name}",
                timeout=120,
            ) as response:
                shutil.copyfileobj(response, temporary)
            temporary.flush()
            if hashlib.sha256(Path(temporary.name).read_bytes()).hexdigest() != SHA256:
                raise ValueError("Sparkle SDK checksum mismatch")
            shutil.copyfile(temporary.name, archive)
    if hashlib.sha256(archive.read_bytes()).hexdigest() != SHA256:
        raise ValueError("Cached Sparkle SDK checksum mismatch; remove it and retry")
    # Extract freshly from the verified archive; never execute stale cached tools.
    directory = tempfile.TemporaryDirectory(prefix="sdk-", dir=cache)
    subprocess.run(["/usr/bin/tar", "-xf", str(archive), "-C", directory.name], check=True)
    return directory


def info():
    return dict(
        GopherUpdatesEnabled=True,
        SUFeedURL=FEED_URL,
        SUPublicEDKey=PUBLIC_KEY,
        SUEnableAutomaticChecks=True,
        SUAutomaticallyUpdate=True,
        SUScheduledCheckInterval=86400,
        SUEnableSystemProfiling=False,
        SUVerifyUpdateBeforeExtraction=True,
        SURequireSignedFeed=True,
        SUSignedFeedFailureExpirationInterval=0,
    )


def embed(app, sdk_path, identity):
    framework = app / "Contents/Frameworks/Sparkle.framework"
    framework.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(["/usr/bin/ditto", str(sdk_path / "Sparkle.framework"), str(framework)], check=True)
    version = framework / "Versions/B"
    # Sign nested code inside-out; never use --deep as a signing shortcut.
    for target in [*sorted((version / "XPCServices").glob("*.xpc")),
                   version / "Autoupdate", version / "Updater.app", framework]:
        subprocess.run(["/usr/bin/codesign", "--force", "--sign", identity,
                        "--preserve-metadata=entitlements", str(target)], check=True)
    resources = app / "Contents/Resources"
    shutil.copy2(sdk_path / "LICENSE", resources / "Sparkle-LICENSE")


def validate_feed(feed, archive, version):
    ns = "{http://www.andymatuschak.org/xml-namespaces/sparkle}"
    items = ET.parse(feed).findall("./channel/item")
    if len(items) != 1:
        raise ValueError("Expected exactly one release in the generated feed")
    item = items[0]
    major, minor, patch = version.split(".")
    expected_build = f"{int(major) + 1}.{minor}.{patch}"
    enclosure = item.find("enclosure")
    if (item.findtext(ns + "version") != expected_build
            or item.findtext(ns + "shortVersionString") != version
            or enclosure is None
            or enclosure.get("url") != f"https://github.com/mbuvarp/gopher/releases/download/v{version}/{archive.name}"
            or enclosure.get("length") != str(archive.stat().st_size)):
        raise ValueError("Generated feed does not match the release archive")
    signature = enclosure.get(ns + "edSignature", "")
    if len(base64.b64decode(signature, validate=True)) != 64:
        raise ValueError("Generated feed is missing the archive signature; check the signing key")
    return signature


def appcast(archive, version, sdk_path, destination, key_file=None, notes=None):
    """Create one signed release item. GOP-4 publishes it with its immutable archive."""
    with tempfile.TemporaryDirectory(prefix="appcast-", dir=destination.parent) as temporary:
        stage = Path(temporary)
        shutil.copy2(archive, stage / archive.name)
        if notes is not None:
            (stage / archive.with_suffix(".md").name).write_text(notes)
        key = ["--ed-key-file", str(key_file)] if key_file else ["--account", KEY_ACCOUNT]
        subprocess.run([str(sdk_path / "bin/generate_appcast"), *key,
                        "--maximum-deltas", "0", "--embed-release-notes", "--download-url-prefix",
                        f"https://github.com/mbuvarp/gopher/releases/download/v{version}/",
                        str(stage)], check=True)
        feed = stage / "appcast.xml"
        if not feed.exists():
            raise ValueError("Sparkle did not generate the required signed feed")
        signature = validate_feed(feed, archive, version)
        subprocess.run([str(sdk_path / "bin/sign_update"), *key, "--verify", str(archive), signature], check=True)
        subprocess.run([str(sdk_path / "bin/sign_update"), *key, "--verify", str(feed)], check=True)
        shutil.copy2(feed, destination)
