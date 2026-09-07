# Gopher

Gopher is a personal macOS menu bar app that monitors GitHub PRs, replacing repeated browser checks while agent reviewers work. This file records the agreed design; components below may not be implemented yet.

## Product behavior

- List open PRs authored by, assigned to, or requesting review from the authenticated user, grouped by repository.
- Give each PR a submenu with **Open PR** and an **Acknowledge update** checkbox.
- Acknowledgement silences that update's contribution to the main menu bar icon. Reset it when a meaningful new update arrives.
- Reflect unacknowledged comments or approvals in the main icon. Notify when a review run finishes with findings or approval; defer findings notifications until all participating reviewers finish.
- Clicking a review notification opens the PR in the default browser and acknowledges that specific update. An older notification must not acknowledge newer updates.

## Review states

- **Unknown:** Evidence is missing, ambiguous, or insufficient to establish the current result.
- **Reviewing:** A participating reviewer is still running. This takes precedence over comments already posted.
- **Comments:** All participating reviewers have finished and unresolved review threads remain.
- **Approved:** All participating reviewers have finished with a clean result for the current changes, and no unresolved threads remain.

Detect participating agents per PR; subscriptions and repository settings vary. Support repository overrides for expected reviewers. Track the current commit and observed review runs, including reruns on the same commit. Never carry an old approval forward without current evidence, or interpret missing activity as approval.

Keep reviewer detection in separate modules. Treat these observed conventions as evidence to validate, not guaranteed API contracts:

- **Cubic:** A running PR check indicates review activity. Review summaries include `N issues found`, `No issues found`, or `0 issues found`; summaries may be edited after findings are addressed.
- **CodeRabbit:** Uses PR checks for activity; verify its completion signals before implementing approval detection.
- **Codex:** Uses an `eyes` reaction on the PR description while reviewing, often with an edited `Codex Review Summary` comment. It may remove `eyes` when posting findings or add `+1` for approval. Attribute reactions to the agent; reactions alone do not identify the reviewed commit.

## Architecture

- Rust; native menus via `tray-icon`/`muda`, background work via `tokio`, parsing via `serde`.
- Use authenticated `gh api graphql` and REST calls. Require an installed, authenticated GitHub CLI; support common install locations and a configured executable path.
- Poll tracked PRs about every 30 seconds; discovery may be less frequent. Batch requests, paginate fully, apply timeouts/backoff, and inspect edited summaries and reactions directly.
- Run one background `.app` in the user session, with launch at login through `SMAppService`. Use native UserNotifications through `objc2` for notification clicks. Keep polling and disk work off the UI thread.
- Show missing-CLI/authentication errors in both the menu and notifications, deduplicating repeated failures. Distinguish network failures. Request notification permission on first launch.

## Persistence and diagnostics

- Store everything under `~/.config/gopher`; do not use macOS Application Support defaults.
- Use `state.sqlite3` via `rusqlite` for cached PR data, reviewer evidence/runs, update identifiers, acknowledgements, and notification history. GitHub remains authoritative; show restored data as stale until refreshed.
- Keep optional settings in `config.toml`.
- Write structured JSONL logs to `logs/gopher.jsonl` using `tracing`, retaining at most **10,000 lines across retained logs**. Bound individual entry sizes and safely replace files when trimming.
- Log request timing/failures, detection evidence, state transitions, notifications, and acknowledgements with timestamps, severity, and PR/reviewer identifiers. Exclude credentials and full API responses by default.

## Development guidance

Prefer structural symbol navigation for unfamiliar code when available; use text search for strings and cross-file usages. Test reviewer parsing and state transitions with representative fixtures, especially stale approvals, edited summaries, overlapping runs, and acknowledgement races. Document actual build/test commands here once established.

## Git

Use Conventional Commits: `type(scope): description`, with an optional scope. Choose a descriptive type such as `feat`, `fix`, `docs`, `refactor`, `test`, or `chore`, and keep the subject concise and imperative.
