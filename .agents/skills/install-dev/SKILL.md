---
name: install-dev
description: Build and install Gopher locally from the current git checkout, then relaunch and verify it. Use for development installs rather than installing a published GitHub release.
---

# Install Gopher Dev

Install the current checkout into `~/Applications/Gopher Dev.app` using the local
development bundle (`dev.mbuvarp.gopher.dev`), including uncommitted source changes.
Never stop or replace production `~/Applications/Gopher.app` or modify
`~/.config/gopher` during installation. Preserve the branch,
working tree, `~/.config/gopher-dev`, notification permissions, and launch-at-login preferences.
Do not switch branches, pull, commit, push, or publish as part of installation.

## Build

1. Locate the Gopher repository and read its `AGENTS.md`. Inspect `git status` and
   record the current branch/commit so the installed source can be identified.
2. From the repository root, run `sh scripts/bundle.sh` without `--release`.
   Reuse the build script's signing-identity selection and bundle validation.
   Build before stopping the installed app; a build failure leaves it running.
3. Verify the bundle identifier is `dev.mbuvarp.gopher.dev` and the display name
   is `Gopher Dev`.
4. Use the freshly generated `dist/Gopher Dev.app`. Do not use the public installer,
   the `gopher install` command (which can skip equal versions), or a downloaded
   release. A development bundle does not embed Sparkle.

## Replace and launch

- Create `~/Applications` if needed. Reject a symlink destination. Stage a fresh
  copy with `ditto` into a temporary directory on the same filesystem, and verify
  it with `codesign --verify --deep --strict` before touching the installed app.
  Do not overlay the old bundle: removed files and release-only frameworks must
  not survive a development install.
- Serialize replacement using an exclusive, nonblocking lock on
  `~/Applications/.gopher-dev-installer.lock`.
- Discover the running Gopher Dev PID and verify its executable path before sending
  SIGTERM. Allow graceful shutdown to drain submitted mutations and persist state.
  Wait up to 150 seconds, yielding for progress updates. Never use SIGKILL; if it
  cannot finish, leave the installed bundle intact and report the blocker.
- After shutdown, hold an exclusive, nonblocking lock on
  `~/.config/gopher-dev/gopher.lock` through replacement (Python `fcntl.flock` is
  compatible with the app's lock). Create the directory/lock if missing without
  truncating existing files. If the lock is unavailable, do not replace the app.
  Rename the existing bundle to a temporary backup, then rename the staged bundle
  into place. Restore the backup if replacement fails. Support a fresh installation
  when there is no existing bundle.
- Release the instance lock before `open "$HOME/Applications/Gopher Dev.app"`. Launch the
  installed bundle, not the build output. If launch fails, retain the backup and
  report the failure; do not silently roll back after the new app may have run.

## Verify and report

Confirm the installed binary matches the build output, its signature verifies,
and the running process uses `~/Applications/Gopher Dev.app/Contents/MacOS/gopher`.
Inspect recent lifecycle/polling logs for successful startup or actionable errors;
do not print credentials or full API responses. Keep any temporary backup until
verification succeeds, then remove only the backup created by this installation.
Report the installed branch/commit (and whether it included uncommitted changes),
launch result, and any build or runtime warnings.

## First-time setup

Dev uses `~/.config/gopher-dev/config.toml` and `state.sqlite3`. Preserve existing
Dev data. Do not automatically copy production state. When the user requests a
copy, stop production first or use SQLite backup; omit locks, logs and session.json.
A copied Open Gopher global shortcut may conflict with production: let the user
clear or change it in Dev Settings. Notification permission and launch-at-login
registration belong to Dev separately; do not change production preferences.
