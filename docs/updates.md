# Updating and signing Gopher

Release packaging (`sh scripts/bundle.sh --release`) downloads Sparkle 2.9.6 into
`target/sparkle`, verifies its pinned SHA-256, extracts it freshly, and embeds the
framework and helpers. Ordinary `cargo` commands and local bundles do not need
Sparkle installed or downloaded. Keep Sparkle's license in each release bundle.

The same ZIP is used by install.sh and Sparkle. Apple signing still uses Gopher's
configured identity/team. Sparkle adds Ed25519 verification before extraction and
requires signed appcasts. The public key lives in scripts/sparkle.py; the private key
is in the developer's Keychain under account `dev.mbuvarp.gopher`. Do not regenerate
it during builds. Back it up securely before distributing the first release.
For CI, export to protected secret storage and materialize a private, temporary key
file, passing its path through `GOPHER_UPDATE_KEY_FILE`; never print the key or pass
it as a command-line argument. Release publishing/secrets setup belongs to GOP-4.

Packaging emits `appcast.xml` alongside the ZIP, checksum, and installer. It validates
that the signed feed identifies the exact version, build number, URL, archive length,
and archive signature. The feed URL is
`https://github.com/mbuvarp/gopher/releases/latest/download/appcast.xml`, while enclosure
URLs name immutable versions. GOP-4 must publish all assets together from a complete
draft release. Do not edit feeds after signing or overwrite published archives. No
delta updates are generated yet. Changelog-derived notes will be added by GOP-4.

## Manual isolated validation

Use another Mac and separate signed test bundles, not the installed Gopher or its data.
The initial integration was tested on the Apple Silicon Mini with Apple Development
signing and no notarization. Browser-download Gatekeeper behavior remains separate.

1. Build the release and create two test copies with a unique bundle ID and increasing
   CFBundleVersion/CFBundleShortVersionString. Preserve Sparkle settings/public key.
2. Give them a loopback-only test SUFeedURL. Add NSAllowsLocalNetworking only to these
   fixtures, re-sign them, zip the newer copy, and generate/sign a matching appcast
   using the pinned SDK's generate_appcast. Never publish the test feed on GitHub.
3. Install the older fixture into ~/Applications. Use an isolated config directory
   and a fake gh executable for a full-app test; no live GitHub mutations.
4. Confirm the update is downloaded without a popup or restart, quit normally, and
   verify the newer version is installed and remains stopped with completed session
   logging and unchanged application data.
5. For an automated explicit relaunch test, compile `cargo build --release --target
   aarch64-apple-darwin --example update_smoke` (MACOSX_DEPLOYMENT_TARGET=13.0).
   Use that binary in both fixture apps, a bundle identifier beginning with
   `dev.mbuvarp.gopher.update-smoke.`, build versions 1.1.0 and 1.1.1, and an absolute
   GopherSmokeDirectory pointing to a fresh temporary directory. This host never
   starts Gopher's worker or accesses GitHub. It waits for a downloaded update,
   invokes the real Install and Relaunch button, delays simulated persistence by
   three seconds, then verifies persistence and cleanup markers on relaunch.
6. Repeat with a corrupted feed/signature, corrupted ZIP, older offered version,
   and unavailable feed. Installed files must remain usable and unchanged.
7. Quit all test applications and helper processes and stop the loopback server.

Ordinary Rust/Python checks cover release metadata, feed/archive consistency, corrupt
SDK rejection, incremental mutations, account changes, and draining/persisting queued
label results during shutdown. Runtime updater failures include domain/code in JSONL;
Sparkle's own helper diagnostics are also available in macOS Console.
