# First-release validation

GOP-7 validates installation and updates before the first public release. Do not
publish fixture versions to the production release feed. The public `curl | bash`
command needs a final check after publication, because its release assets do not
exist beforehand.

Pre-publication validation completed on 2026-09-11. The browser trust prompt is an
expected part of the chosen unnotarized distribution, and is documented in the
installation instructions.

## Environment and recovery

- Candidate: `0.1.0`, commit `736f4a0a6d5678934ae8adbf4bf84b83de7f23d9`.
- Independent host: Apple Silicon Mac Mini, macOS 26.5.1.
- Signing policy: Apple Development signing, no notarization; preserve quarantine
  and use the normal macOS trust flow.
- Before replacing the real app, stop Gopher and its updater, back up the app and
  `~/.config/gopher`, export `dev.mbuvarp.gopher` defaults, and verify the copies.
  Record whether the app was running and whether launch at login was enabled.
- Use synthetic PRs and an offline GitHub CLI fixture for repeated UI tests.
  Check real CLI discovery/authentication separately. Never run fixture mutations
  against GitHub.
- After testing, disable test login items, stop all fixture apps and updater
  helpers, restore the original app/data/preferences, and stop the local feed
  server. `defaults import` merges keys: remove only the backed-up Gopher domain
  before importing it, so fixture-only keys do not survive. Verify the restored
  dictionary and file hashes. Leave the Mini's Gopher stopped.

## Validation record

| Area | Evidence / result |
| --- | --- |
| Rust checks | Formatting and Clippy passed; 153 tests passed on the candidate code. |
| Installer failure boundaries | All 32 Python tests passed, including bad signatures/checksums, unsafe archives, mismatched versions, failed authentication, and unavailable/interrupted downloads without invoking installation. |
| Replacement safeguards | Rust tests cover replacement rollback, version reinspection, symlink rejection, installer locking, and overlap with Sparkle. |
| Workflow rules | Python tests cover version ordering, first-release handling, missing/empty notes, mismatched lock versions, approved-SHA guards, draft recovery, and incomplete/corrupt uploads. |
| GitHub signed packaging | [Validation run](https://github.com/mbuvarp/gopher/actions/runs/34546552332) passed. Downloaded artifact receipt matches the candidate SHA/version and all four asset hashes; extracted app passes strict Apple signing-team/identifier verification. No tag/release published. |
| Browser download and trust | Passed: user downloaded the CI artifact in a browser, extracted and moved Gopher to `~/Applications`, and opened it through Privacy & Security approval. Apple displayed the expected cannot-verify-malware warning in both `/Applications` and `~/Applications`; changing the folder does not bypass trust. The downloaded app's signature verified and its quarantine marker was retained. |
| Fresh installation | User confirmed the popover, launch-at-login toggle, and review notification click opening/highlighting PR #9. Native notification permission is granted; two synthetic review notifications were scheduled. SQLite confirmed #9's acknowledgement and #8's ignored state. Real installed `gh auth status` succeeded, and the upgraded app's `doctor` found `/opt/homebrew/bin/gh` and authenticated successfully. Missing/unauthenticated CLI handling is covered by automated tests. |
| Installer rerun / upgrade | Passed: current-version rerun left the running app alone; a stopped `0.0.9` fixture upgraded and remained stopped; a running `0.0.9` fixture shut down cleanly, upgraded and relaunched with a new PID. PR acknowledgements, ignored PRs, hotkeys, config, and updater preferences survived. Only GitHub release discovery/download transport was simulated; checksum, archive, signature, and bundled Rust helper were real. |
| Signed in-app upgrade | Passed: full `0.0.9` fixture downloaded the unmodified `0.1.0` CI archive from a signed loopback feed, installed on orderly SIGTERM shutdown, and remained stopped. Session completion, strict app signature, PR acknowledgements, ignored PRs, shortcut preferences, and config checksum were verified afterward. Explicit relaunch and the normal menu Quit path also passed in isolated smoke hosts. |
| Isolated updater scenarios | Eight scenarios passed on the Mini: install-on-quit, explicit relaunch after delayed persistence, retained update reminder, downgrade/no-update, unavailable feed, corrupted feed, corrupted archive, and interrupted archive download. All final app signatures passed verification. |
| Post-upgrade desktop checks | User confirmed notification clicks still open/highlight PR #9 and launch at login remains enabled after upgrading. The new acknowledgement was verified in SQLite. User then disabled launch at login and quit; session completion was verified. |
| Cleanup | Original Mini app/data restored and checked byte-for-byte against the backup; original Gopher defaults restored and compared as dictionaries. Smoke apps removed, local feed server stopped, and no Gopher app/updater processes left running. Backup retained on the Mini. |

The isolated smoke hosts upgraded from `0.1.0` to `0.1.1` in the quit and explicit
relaunch scenarios. The quit scenario remained stopped; the relaunch scenario
verified both persistence and clean-shutdown markers before exiting. Negative
scenarios retained `0.1.0`; the downgrade host retained its newer `0.1.2`. These
are local fixture versions, not Git tags or published releases.

## Repeating the checks

Run the Rust/Python checks from [AGENTS.md](../AGENTS.md). Dispatch the Release
workflow in `validate` mode with the full approved `expected_sha`; successful
validation creates an Actions artifact, not a tag or release. Follow the exact
run to completion, download its artifact, and validate the receipt against the
candidate version and commit.

Use the [isolated Sparkle procedure](updates.md#manual-isolated-validation) and
`examples/update_smoke.rs` for failure scenarios. These fixtures use unique app
identities, loopback feeds and temporary data. They complement a full-app test
using the production identity and a backed-up user installation; the smoke host
alone does not verify PR data, notifications, or launch at login.

Record manual observations separately from automated assertions. Passing on the
Mini does not establish compatibility with every supported macOS version; macOS
13 compatibility also relies on the deployment target and bundle validation.
