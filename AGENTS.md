# Gopher

Gopher is a personal macOS menu bar app that monitors GitHub PRs, replacing repeated browser checks while agent reviewers work.

## Product behavior

- List open PRs authored by, assigned to, or requesting review from the authenticated user, grouped by repository. Headings use `repo • organization`; sort by organization, repository, then ascending PR number.
- Keep an open menu stable during background updates; apply the latest state after it closes and preserve the update IDs of actions the user actually saw.
- Show PR review states with native template image icons (SF Symbols), not Unicode prefixes; retain text status labels inside each submenu.
- Give each PR a submenu with **Open PR**, an **Acknowledge update** checkbox, and **Ignore PR**. Opening a PR successfully acknowledges the displayed update. Clicking the PR row itself also acknowledges that displayed update; hovering still opens its submenu.
- Ignore permanently hides a specific PR from polling, menus, and notifications. Persist its GitHub node ID independently of cache pruning or account changes; discard any in-flight result for it.
- Acknowledgement silences that update's contribution to the main menu bar icon. Reset it when a meaningful new update arrives.
- Use the neutral monochrome cartoon gopher (`assets/gopher.png`) for both idle and review-in-progress menu bar states; background polling alone does not change the icon. Reserve the question mark for errors or unacknowledged unknown/stale PRs.
- Reflect unacknowledged comments or approvals in the main icon. Notify when a review run finishes with findings or approval; defer findings notifications until all participating reviewers finish.
- Clicking a review notification or its **Acknowledge** action acknowledges that specific update without opening the browser. The **Open PR** notification action opens the PR and acknowledges only after the browser opens successfully. An older notification must not acknowledge newer updates.

## Review states

- **Unknown:** Evidence is missing, ambiguous, or insufficient to establish the current result.
- **Reviewing:** A participating reviewer is still running. This takes precedence over comments already posted.
- **Comments:** All participating reviewers have finished and unresolved review threads remain.
- **Approved:** All participating reviewers have finished with a clean result for the current changes, and no unresolved threads remain.

Detect participating agents per PR; subscriptions and repository settings vary. Support repository overrides for expected reviewers. Track the current commit and observed review runs, including reruns on the same commit. Never carry an old approval forward without current evidence, or interpret missing activity as approval.

Exclude explicit subscription-limit/paused-review skips from inferred participation, while displaying the reason. An explicitly required reviewer that skips keeps the result Unknown. Confirm actionable results across polls before notifying (30-second default).

Keep reviewer detection in separate modules. Treat these observed conventions as evidence to validate, not guaranteed API contracts:

- **Cubic:** A running PR check indicates review activity. Review summaries include `N issues found`, `No issues found`, or `0 issues found`; summaries may be edited after findings are addressed.
- **CodeRabbit:** Uses checks or legacy commit statuses for activity. Require an explicit current-commit review verdict; successful or skipped statuses alone are not approval.
- **Codex:** Uses an `eyes` reaction on the PR description while reviewing, often with an edited `Codex Review Summary` comment. It may remove `eyes` when posting findings or add `+1` for approval. Attribute reactions to the agent; reactions alone do not identify the reviewed commit.

## Architecture

- Rust; native menus via `tray-icon`/`muda`, background work via `tokio`, parsing via `serde`.
- Use authenticated `gh api graphql` and REST calls. Require an installed, authenticated GitHub CLI; support common install locations and a configured executable path.
- Poll tracked PRs about every 30 seconds; discovery may be less frequent. Batch requests, paginate fully, apply timeouts/backoff, and inspect edited summaries and reactions directly.
- Run one background `.app` in the user session, with launch at login through `SMAppService`. Use native UserNotifications through `objc2` for notification clicks. Keep polling and disk work off the UI thread.
- Show missing-CLI/authentication errors in both the menu and notifications, deduplicating repeated failures. Distinguish network failures. Request notification permission on first launch.

## Persistence and diagnostics

- Store everything under `~/.config/gopher`; do not use macOS Application Support defaults.
- Use `state.sqlite3` via `rusqlite` for cached PR data, reviewer evidence/runs, update identifiers, acknowledgements, ignored PR IDs, and notification history. GitHub remains authoritative; show restored data as stale until refreshed.
- Keep optional settings in `config.toml`.
- Write structured JSONL logs to `logs/gopher.jsonl` using `tracing`, retaining at most **10,000 lines across retained logs**. Bound individual entry sizes and safely replace files when trimming.
- Log request timing/failures, detection evidence, state transitions, notifications, and acknowledgements with timestamps, severity, and PR/reviewer identifiers. Exclude credentials and full API responses by default.

## Development guidance

Prefer structural symbol navigation for unfamiliar code when available; use text search for strings and cross-file usages. Test reviewer parsing and state transitions with representative fixtures, especially stale approvals, edited summaries, overlapping runs, and acknowledgement races.

- Check: `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`.
- Diagnose: `cargo run -- doctor`, `cargo run -- inspect OWNER/REPO NUMBER` (read-only; no notifications).
- Package: `sh scripts/bundle.sh`. Notifications need the `.app` bundle and reliable Apple signing; the script prefers an installed Apple Development identity. Use `GOPHER_SIGNING_IDENTITY` to override.

## Git

Use Conventional Commits: `type(scope): description`, with an optional scope. Choose a descriptive type such as `feat`, `fix`, `docs`, `refactor`, `test`, or `chore`, and keep the subject concise and imperative.
