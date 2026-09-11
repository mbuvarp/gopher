---
name: install-dev
description: Build and install Gopher locally from the current git checkout, then relaunch and verify it. Use for development installs rather than installing a published GitHub release.
---

# Install Gopher Dev

Install the current checkout into `~/Applications/Gopher.app` using the local
development bundle. This includes uncommitted source changes. Preserve the branch,
working tree, `~/.config/gopher`, notification permissions, and launch-at-login preferences.
Do not switch branches, pull, commit, push, or publish as part of installation.

## Build

1. Locate the Gopher repository and read its `AGENTS.md`. Inspect `git status` and
   record the current branch/commit so the installed source can be identified.
2. From the repository root, run `sh scripts/bundle.sh` without `--release`.
   Reuse the build script's signing-identity selection and bundle validation.
   Build before stopping the installed app; a build failure leaves it running.
3. Use the freshly generated `dist/Gopher.app`. Do not use the public installer,
   the `gopher install` command (which can skip equal versions), or a downloaded
   release. A development bundle does not embed Sparkle.

## Replace and launch

- Create `~/Applications` if needed. Reject a symlink destination. Stage a fresh
  copy with `ditto` into a temporary directory on the same filesystem, and verify
  it with `codesign --verify --deep --strict` before touching the installed app.
  Do not overlay the old bundle: removed files and release-only frameworks must
  not survive a development install.
- Serialize replacement using an exclusive, nonblocking lock on
  `~/Applications/.gopher-installer.lock`. Check for an active Sparkle installation
  before shutdown and again before replacement; use `ensure_no_update_installer`
  in `src/installer.rs` as the reference for identifying Gopher's updater process.
  Report a conflict without replacing.
- Discover the running Gopher PID and verify its executable path before sending
  SIGTERM. Allow graceful shutdown to drain submitted mutations and persist state.
  Wait up to 150 seconds, yielding for progress updates. Never use SIGKILL; if it
  cannot finish, leave the installed bundle intact and report the blocker.
- After shutdown, hold an exclusive, nonblocking lock on
  `~/.config/gopher/gopher.lock` through replacement (Python `fcntl.flock` is
  compatible with the app's lock). Create the directory/lock if missing without
  truncating existing files. If the lock is unavailable, do not replace the app.
  Rename the existing bundle to a temporary backup, then rename the staged bundle
  into place. Restore the backup if replacement fails. Support a fresh installation
  when there is no existing bundle.
- Release the instance lock before `open ~/Applications/Gopher.app`. Launch the
  installed bundle, not the build output. If launch fails, retain the backup and
  report the failure; do not silently roll back after the new app may have run.

## Verify and report

Confirm the installed binary matches the build output, its signature verifies,
and the running process uses `~/Applications/Gopher.app/Contents/MacOS/gopher`.
Inspect recent lifecycle/polling logs for successful startup or actionable errors;
do not print credentials or full API responses. Keep any temporary backup until
verification succeeds, then remove only the backup created by this installation.
Report the installed branch/commit (and whether it included uncommitted changes),
launch result, and any build or runtime warnings.
