# Changelog

Release entries use `## [major.minor.patch] - YYYY-MM-DD`, followed by the
user-facing changes for that version. The release workflow uses that entry for
both GitHub release notes and Gopher's update dialog.

## [Unreleased]

## [0.2.0] - 2026-09-12

- Show confirmed merge conflicts in PR status and prevent conflicting merges.
- Show source and destination branches in PR details, with clearer reviewer headings.
- Acknowledge the displayed update when starting a merge or opening the label picker.
- Prevent accidental selection of check-status text.
- Use modern macOS Tahoe controls and popover styling while retaining macOS 13 support.
- Separate local development builds as Gopher Dev, with DEV badges and independent configuration and state, so the released app can remain installed for daily use.

## [0.1.0] - 2026-09-11

Initial release of Gopher for Apple Silicon Macs running macOS 13 or later. Requires an installed, authenticated GitHub CLI (`gh`).

- Monitor your open GitHub PRs in a native menu bar inbox, grouped by repository, with review progress, unresolved threads, labels, and CI status.
- Track Codex, Cubic, and CodeRabbit reviews; receive notifications when reviews finish, acknowledge updates, and ignore or restore individual PRs.
- Navigate with customizable keyboard shortcuts and configure repository-specific merge and label actions.
- Install or update with the same installer script. Signed in-app updates download automatically and install when you quit; manual update checks and update preferences are available in the app.
- Preserve PR state and settings locally, with structured logs for troubleshooting.

This release is unnotarized. macOS may require approval in System Settings → Privacy & Security before the first launch.
