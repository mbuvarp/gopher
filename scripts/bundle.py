"""Build local apps and unnotarized release archives using macOS tools and Python 3."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import subprocess
import sys
import tempfile
import sparkle


ROOT = Path(__file__).resolve().parent.parent
TARGET = "aarch64-apple-darwin"
MINIMUM_OS = "13.0"
RELEASE_IDENTITY = '=anchor apple generic and identifier "dev.mbuvarp.gopher" and certificate leaf[subject.OU] = "DZ4XZQXHZ7"'


def run(*args, **kwargs):
    return subprocess.run(args, check=True, cwd=ROOT, **kwargs)


def output(*args):
    return run(*args, stdout=subprocess.PIPE, text=True).stdout.strip()


def bundle_version(version):
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version):
        raise ValueError("Packaging requires a stable major.minor.patch version")
    major, minor, patch = map(int, version.split("."))
    # A positive leading component also places 0.1.0 after the old build number 1.
    # Respect Apple's four/two/two digit build-component limits.
    if major > 9998 or minor > 99 or patch > 99:
        raise ValueError("Version exceeds supported macOS build-component limits")
    return f"{major + 1}.{minor}.{patch}"


def signing_identity(release):
    identity = os.environ.get("GOPHER_SIGNING_IDENTITY", "").strip()
    if not identity:
        identities = output("security", "find-identity", "-v", "-p", "codesigning")
        match = re.search(r'\b([A-F0-9]{40}) "Apple Development:', identities)
        identity = match.group(1) if match else "-"
    if identity == "-":
        if release:
            raise ValueError(
                "Release packaging requires an Apple signing identity. Install an Apple "
                "Development identity or set GOPHER_SIGNING_IDENTITY; ad-hoc releases are disabled."
            )
        print("Warning: ad-hoc local build; notification authorization may fail.", file=sys.stderr)
    return identity


def write_info(destination, version, release=False):
    with (ROOT / "packaging/Info.plist").open("rb") as source:
        info = plistlib.load(source)
    info.update(
        CFBundleShortVersionString=version,
        CFBundleVersion=bundle_version(version),
        LSMinimumSystemVersion=MINIMUM_OS,
        GopherInstallerProtocol=1,
    )
    if release:
        info.update(sparkle.info())
    with destination.open("wb") as target:
        plistlib.dump(info, target, sort_keys=False)


def verify_bundle(bundle, version, release=False):
    binary = bundle / "Contents/MacOS/gopher"
    if output("lipo", "-archs", str(binary)) != "arm64":
        raise ValueError("Packaged binary must contain only arm64")
    load_commands = output("otool", "-l", str(binary))
    if not re.search(r"\bminos 13\.0(?:\.0)?\s", load_commands):
        raise ValueError("Packaged binary must target macOS 13.0")
    with (bundle / "Contents/Info.plist").open("rb") as source:
        info = plistlib.load(source)
    expected = {
        "CFBundleShortVersionString": version,
        "CFBundleVersion": bundle_version(version),
        "LSMinimumSystemVersion": MINIMUM_OS,
        "CFBundleIdentifier": "dev.mbuvarp.gopher",
    }
    if release:
        expected.update(sparkle.info())
    if any(info.get(key) != value for key, value in expected.items()):
        raise ValueError("Packaged app metadata does not match the Cargo version")
    if not os.access(binary, os.X_OK):
        raise ValueError("Packaged binary is not executable")
    requirement = ["-R", RELEASE_IDENTITY] if release else []
    run("codesign", "--verify", "--deep", "--strict", *requirement, str(bundle))


def publish(staged, destination):
    """Replace all outputs only after verification; restore old outputs on failure."""
    backups = []
    installed = []
    try:
        for source in staged:
            target = destination / source.name
            backup = source.parent / (source.name + ".previous")
            if target.exists():
                target.rename(backup)
                backups.append((backup, target))
            source.rename(target)
            installed.append(target)
    except OSError:
        for target in reversed(installed):
            if target.is_dir():
                shutil.rmtree(target)
            else:
                target.unlink()
        for backup, target in reversed(backups):
            backup.rename(target)
        raise


def build(release):
    if sys.platform != "darwin":
        raise ValueError("Gopher packaging requires macOS and Xcode command-line tools")
    identity = signing_identity(release)
    metadata = json.loads(output("cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"))
    package = next(p for p in metadata["packages"] if Path(p["manifest_path"]).resolve() == ROOT / "Cargo.toml")
    version = package["version"]
    bundle_version(version)
    run("cargo", "build", "--release", "--locked", "--target", TARGET,
        env={**os.environ, "MACOSX_DEPLOYMENT_TARGET": MINIMUM_OS})
    binary = Path(metadata["target_directory"]) / TARGET / "release/gopher"
    dist = ROOT / "dist"
    dist.mkdir(exist_ok=True)
    # Stage on the output filesystem so publication uses renames, never partial copies.
    with tempfile.TemporaryDirectory(prefix=".bundle-", dir=dist) as temporary:
        stage = Path(temporary)
        bundle = stage / "Gopher.app"
        (bundle / "Contents/MacOS").mkdir(parents=True)
        resources = bundle / "Contents/Resources"
        resources.mkdir()
        shutil.copy2(binary, bundle / "Contents/MacOS/gopher")
        write_info(bundle / "Contents/Info.plist", version, release=release)
        iconset = stage / "Gopher.iconset"
        iconset.mkdir()
        for size in (16, 32, 128, 256, 512):
            for scale in (1, 2):
                pixels = str(size * scale)
                suffix = "@2x" if scale == 2 else ""
                run("sips", "-z", pixels, pixels, str(ROOT / "assets/gopher-app.png"),
                    "--out", str(iconset / f"icon_{size}x{size}{suffix}.png"), stdout=subprocess.DEVNULL)
        run("iconutil", "-c", "icns", str(iconset), "-o", str(resources / "Gopher.icns"))
        if release:
            sdk_context = sparkle.sdk()
            sdk_path = Path(sdk_context.name)
            sparkle.embed(bundle, sdk_path, identity)
        run("codesign", "--force", "--sign", identity, str(bundle))
        verify_bundle(bundle, version, release=release)
        products = [bundle]
        if release:
            archive = stage / f"Gopher-{version}-macos-arm64.zip"
            run("ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", str(bundle), str(archive))
            extracted = stage / "verification"
            run("ditto", "-x", "-k", str(archive), str(extracted))
            verify_bundle(extracted / "Gopher.app", version, release=True)
            checksum = stage / (archive.name + ".sha256")
            with archive.open("rb") as source:
                digest = hashlib.sha256()
                for chunk in iter(lambda: source.read(1024 * 1024), b""):
                    digest.update(chunk)
            checksum.write_text(f"{digest.hexdigest()}  {archive.name}\n")
            feed = stage / "appcast.xml"
            notes_file = os.environ.get("GOPHER_RELEASE_NOTES_FILE")
            notes = Path(notes_file).read_text() if notes_file else None
            sparkle.appcast(archive, version, sdk_path, feed, os.environ.get("GOPHER_UPDATE_KEY_FILE"), notes)
            products.extend([archive, checksum, feed])
            sdk_context.cleanup()
            installer = stage / "install.sh"
            shutil.copy2(ROOT / "scripts/install.sh", installer)
            products.append(installer)
        publish(products, dist)
    print(f"Built {dist / 'Gopher.app'} (version {version}, build {bundle_version(version)}).")
    if release:
        print(f"Archive: {dist / archive.name}\nChecksum: {dist / checksum.name}")
        print("Unnotarized build: macOS may require Privacy & Security → Open Anyway.")
    print("Move the app to ~/Applications, open it, then enable Launch at login in its menu.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release", action="store_true", help="also create a certificate-signed, unnotarized ZIP and checksum")
    try:
        build(parser.parse_args().release)
    except (OSError, ValueError, StopIteration, subprocess.CalledProcessError) as error:
        sys.exit(f"Packaging failed: {error}")
