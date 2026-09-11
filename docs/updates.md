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
it as a command-line argument. See the manual release and signing-secret setup below.

Packaging emits `appcast.xml` alongside the ZIP, checksum, and installer. It validates
that the signed feed identifies the exact version, build number, URL, archive length,
and archive signature. The feed URL is
`https://github.com/mbuvarp/gopher/releases/latest/download/appcast.xml`, while enclosure
URLs name immutable versions. The Release workflow publishes all assets together
from a complete draft release. Do not edit feeds after signing or overwrite published archives. No
delta updates are generated yet. The workflow embeds changelog-derived release notes
before the appcast is signed.

## Manual isolated validation

See the [first-release validation record](release-validation.md) for GOP-7's
candidate, results, and recovery procedure.

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
   Set GopherSmokeMode to `menu` to exercise the normal Quit action's drain-and-exit
   path instead; verify the newer app is installed without relaunching. Set it to
   `reminder` with SUAutomaticallyUpdate false to dismiss an undownloaded update
   and verify that the menu reminder remains. Use a fresh bundle ID/data directory
   for each scenario. The host drops Tao's event loop before final termination,
   matching the production cleanup order.
6. Repeat with a corrupted feed/signature, corrupted ZIP, older offered version,
   and unavailable feed. Installed files must remain usable and unchanged.
7. Quit all test applications and helper processes and stop the loopback server.

Ordinary Rust/Python checks cover release metadata, feed/archive consistency, corrupt
SDK rejection, incremental mutations, account changes, and draining/persisting queued
label results during shutdown. Runtime updater failures include domain/code in JSONL;
Sparkle's own helper diagnostics are also available in macOS Console.

## Manual GitHub releases

The **Release** Actions workflow is dispatched on `main`. Its default `validate`
mode checks eligibility, runs tests, and builds/signs the release, then retains the
outputs as a seven-day Actions artifact. It creates no Git tag or GitHub release.
Choose `publish` explicitly to publish. Both modes require the signing secrets and
a release-ready changelog/version; an empty `[Unreleased]` section is not enough.
The workflow must first be merged to the default branch before it can be dispatched.

Version changes remain manual. Update Cargo.toml and Cargo.lock and add exactly one
nonempty `## [major.minor.patch] - YYYY-MM-DD` entry to CHANGELOG.md. Stable numeric
versions must exceed every published stable release; drafts and prereleases do not
establish that baseline. With no published releases, the current valid version can
be the first release. API errors never count as an empty release history. Packaging's
macOS version-component limits also apply. The repository [release skill](../.agents/skills/release/SKILL.md) guides
version/changelog approval. It uses normal direct pushes to main when branch rules
permit them; protected branches follow their required PR/merge flow.

All jobs check out the dispatched commit and verify it belongs to main; main advancing
while a build runs does not change that build. Release runs are serialized. The build
job has read-only repository access; only the separate publish job has contents-write
permission. Signing uses the main-only `release` environment and a temporary keychain.
Repository release immutability must remain enabled (it is configured separately).
GitHub restricts reading that setting to administrators, so the workflow does not
require an admin token; it verifies the published response is immutable and reports
a failure if that confirmation is missing. The three environment secrets are:

- `GOPHER_APPLE_CERTIFICATE_P12`: base64 of the selected Apple Development identity's
  PKCS#12 export, including its private key, for the existing Gopher signing team.
- `GOPHER_APPLE_CERTIFICATE_PASSWORD`: the export's password.
- `GOPHER_SPARKLE_PRIVATE_KEY`: the existing base64 Sparkle key exported by
  `generate_keys --account dev.mbuvarp.gopher -x <private-file>`.

Transfer secrets directly to GitHub secret storage without printing them, putting
private values in command arguments, or committing exported files. Do not generate a
new Sparkle key. macOS may require user interaction to export an Apple identity.

Publication creates a draft for `v<version>` at the exact commit, uploads the ZIP,
checksum, installer, and signed appcast, and compares GitHub's uploaded asset sizes
and SHA-256 digests with the build receipt. Changelog notes are embedded in the signed
appcast and used as GitHub release notes. The complete draft is published as an immutable
release and marked latest. No additional workflow approval is required after choosing
publish. Published archives/tags cannot be replaced; fixes need another version.

A failed upload leaves an unpublished draft. Rerunning the same commit/version can
recover a draft only if it has this workflow's marker, GitHub Actions authorship, and
matching target commit. The workflow replaces the complete expected asset set, verifies
it, and publishes. Unknown assets, different commits, unrelated drafts, or tag collisions
fail for manual inspection; the workflow never deletes such releases or tags. If a code
fix changes the commit, inspect and remove the failed unpublished draft/tag manually
before dispatching that version again. Never delete a published release to reuse its
version. If publication's response is lost, inspect GitHub first; rerunning cannot
modify the now-published release. Validation-only runs never change an existing draft.

Before the first real publication, GOP-7 covers validation mode on GitHub and the
colleague installation checks. Local tests use fake GitHub responses and isolated
signed fixtures; do not publish test versions to the production release feed.

### Approved commit dispatch

The release skill always supplies the full approved commit as `expected_sha` when
dispatching `release.yml` on main. The workflow compares it with GitHub's dispatched
SHA before validation, signing, or publication. An unexpected or malformed value
fails the run. Both build and publish jobs enforce this through `workflow_commit`.
Manual dispatches can leave the optional field empty to retain the existing workflow
behavior. If main changes before the skill dispatches, it must reassess the changes
and approvals; it must not omit the guard or retry blindly.

The skill has two planned approval points: version choice, then the exact changelog
entry. During a release request the latter authorizes commit, push, and publication
of that scope. Preparing/validating alone does not authorize a publish, and incomplete
GOP-7 validation prevents the first publication. Failed or ambiguous dispatches are
inspected before any user-requested retry. Follow the returned run URL, or identify
its exact SHA/event/actor/time; do not attach to an arbitrary latest workflow run.
