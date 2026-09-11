# Gopher

Gopher is a personal macOS menu bar app that monitors GitHub PRs, replacing repeated browser checks while agent reviewers work.

## Interface

Left-click opens the native AppKit popover; right-click opens the alternative standard menu. The popover places Open PR and Details on the left of each row, with a right-aligned Actions dropdown and expandable review details. Unacknowledged actionable updates use bold blue PR titles and blue status icons instead of dots; there is no acknowledgement checkbox. Clicking a PR title acknowledges its displayed update without dismissing the popover. Keep controls and scroll position stable across polls, and update acknowledgement indicators from the worker's persisted state. Actions capture their displayed update ID before dispatch. The header Actions menu starts with Show ignored; Refresh is only in the header. The ignored view replaces the active list, shows Restore instead of Ignore, and replaces Refresh/Actions with Back. Preserve each view’s scroll position. Archive ignored PR details separately from the active cache. Check ignored PR identities and open/closed state at startup and every 15 minutes, without fetching reviews. Hide confirmed closed or merged PRs, retaining ignore flags so reopened PRs remain ignored and reappear in this view. Failed or unavailable lookups must not be interpreted as closure. Restore removes the persistent ignore flag and triggers fresh discovery, discarding older in-flight review results.

## Settings and keyboard navigation

Settings replaces the inbox with a Hotkeys panel and Back button. Fixed, non-configurable Command+, opens Settings from the focused popover, including subpanels; reserve it against custom bindings, preserve save guards and inbox scroll position, and do not recreate an already open Settings panel. Keep advanced settings in `config.toml`, accessible through Open configuration file; file changes still require restart. Persist application-wide shortcut overrides in SQLite independently of account/cache changes. Record, clear, or restore shortcuts without Save/restart. Reject duplicates and unsupported keys; Open Gopher requires Command, Option, or Control. Register its new global binding before saving, keep the previous binding active until persistence succeeds, and roll back registration on failure. Surface errors inline and keep disk work on the worker.

Only Open Gopher is global (unset by default) and toggles the popover open/closed; inbox defaults are Next PR J, Previous PR K, Acknowledge Space, Open PR O, Toggle Details D, Open Actions A, Refresh R. Local shortcuts run only in the focused inbox, never while editing, recording, or tracking a menu. Escape is non-configurable: cancel recording or dismiss a native dropdown first; otherwise return any subpanel or ignored list to the active inbox, or close the popover from that inbox. Preserve save guards and ignore key repeats. Repeat only navigation. Open Actions opens/focuses the native dropdown for arrow-key navigation. Preserve existing displayed-update action semantics and disabled controls.

Highlight by stable PR identity with a rounded white background at 8% opacity, extending 8 points beyond the content on each side, separate from acknowledgement. Navigate in displayed order; from none select first/last, and moving beyond either end returns to none. Scroll highlighted PRs into view. Clear highlight on reopening or target removal; preserve it across polls and when returning from subpanels, separately for active/ignored lists. Ignored PRs support navigation, Open PR, and Details; Acknowledge, Actions, and Refresh are unavailable. Notification-body clicks acknowledge as before, then reveal and highlight their target. Flash that target’s title and status icon between blue and default three times at 300 ms per phase, then restore the current acknowledgement color. Keep this cue UI-only, cancel on navigation/closure/removal, and restart for a new notification; defer navigation while recording or saving settings as well as existing label/menu guards.

## Product behavior

- List open PRs authored by, assigned to, or requesting review from the authenticated user, grouped by repository. Headings use `repo • organization`; sort by organization, repository, then ascending PR number.
- Keep the alternative standard menu stable during background updates; apply the latest state after it closes and preserve the update IDs of actions the user actually saw.
- Show PR review states with native template image icons (SF Symbols), not Unicode prefixes; retain text status labels inside each submenu.
- In the alternative standard menu, give each PR a submenu with **Open PR**, an **Acknowledge update** checkbox, and **Ignore PR**. Opening a PR successfully acknowledges the displayed update. Clicking the PR row itself also acknowledges that displayed update; hovering still opens its submenu.
- Ignore hides a specific PR from review polling, the active list, and notifications until explicitly restored. Persist its GitHub node ID independently of cache pruning or account changes; discard any in-flight result for it.
- Acknowledgement silences that update's contribution to the main menu bar icon. Reset it when a meaningful new update arrives.
- Show elapsed local observation time beside Reviewing, flooring to whole minutes (`Reviewing (7m)`, `Reviewing (1h 2m)`). Persist the start across polls/restarts; reset on a new commit or when entering Reviewing again. Elapsed time must not change update IDs or notifications.
- Append a colored check summary to the popover detail row: Checks running... (yellow), Checks failed (red), or Checks green (green). Aggregate all current-head check runs and latest legacy statuses per context using the existing paginated responses. Failure takes precedence over running; success, neutral, skipped, and no checks are green. Persist the summary separately from review evidence without changing review update IDs, acknowledgements, or notifications. Hide it for stale, ignored, or older snapshots without a summary.
- Use the neutral monochrome cartoon gopher (`assets/gopher.png`) for both idle and review-in-progress menu bar states; background polling alone does not change the icon. Reserve the question mark for errors or unacknowledged unknown/stale PRs.
- Reflect unacknowledged comments or approvals in the main icon. Notify when a review run finishes with findings or approval; defer findings notifications until all participating reviewers finish.
- Clicking a review notification or its **Acknowledge** action acknowledges that specific update without opening the browser. The **Open PR** notification action opens the PR and acknowledges only after the browser opens successfully. An older notification must not acknowledge newer updates.
- Clicking the notification body also opens the active PR list in the popover and scrolls the target PR into view, without closing an already open popover. Send navigation after persisted acknowledgement state reaches the UI. Defer leaving menus or label pickers while saving; preserve visible label failures. Missing or closed targets open the inbox without restoring ignored PRs. Explicit notification actions retain their behavior.

## PR actions

Show assigned labels after the unresolved-thread count as compact colored pills using the same 11-point font. Wrap overflow above the buttons. Fetch labels with normal paginated PR snapshots and persist them; label changes must not alter review update IDs or acknowledgements. Apply successful local label changes immediately and discard older in-flight poll results.

- The PR Actions menu contains Configure, a separator, enabled actions, a separator, then Ignore. Hide the redundant separator when no optional actions are enabled. Keep the alternative standard menu unchanged.
- Configure replaces the list with `Configure repo • organization` and a right-aligned Back button. The table columns are Action, Enabled, Condition, and Merge method (empty for Label). Save repository-wide changes immediately to SQLite; preserve settings across account changes and cache pruning.
- Both actions default disabled. Conditions are Always, Reviewing, Comments, Approved; a configured action remains visible but disabled when its condition fails or data is stale. Merge defaults to Approved and Merge commit; Label defaults to Always. Support Squash and Rebase as merge methods.
- Merge starts a worker-owned five-second Cancel countdown that survives closing menus/popovers and navigation, but not quitting. Capture the displayed head, revalidate account, settings, current PR state and head before submitting, and send the expected SHA to GitHub. Cancel on changed evidence before submission. Report GitHub blockers; never silently queue, auto-merge, bypass protection, or automatically retry mutations.
- Label opens a persistent picker with colored dots and checked assigned labels. Fully paginate available/assigned labels; queue incremental add/remove mutations so multiple toggles preserve unrelated labels. Keep the panel open while saving and show failures. Use simulated GitHub mutations in tests. Open the picker from a persistent, account-scoped repository label catalogue without fetching. Refresh catalogues once per repository about every 15 minutes (including first discovery), coalesce manual Refresh labels requests, and retain cached/applied labels during refreshes or failures. Derive selections from PR snapshots with pending/completed local mutation overlays until LabelSaved updates the snapshot; never overwrite pending toggles with a catalogue response.

## Review states

- **Unknown:** Evidence is missing, ambiguous, or insufficient to establish the current result.
- **Reviewing:** A participating reviewer is still running. This takes precedence over comments already posted.
- **Comments:** All participating reviewers have finished and unresolved review threads remain.
- **Approved:** All participating reviewers have finished with a clean result for the current changes, and no unresolved threads remain.

Detect participating agents per PR; subscriptions and repository settings vary. Support repository overrides for expected reviewers. Track the current commit and observed review runs, including reruns on the same commit. Never carry an old approval forward without current evidence, or interpret missing activity as approval.

Exclude explicit subscription-limit/paused-review skips and Cubic’s automatic-review skips after branch rewrites from inferred participation, while displaying the reason. For a branch rewrite, explain that a manual Cubic review is required to review the rewritten content. An explicitly required reviewer that skips keeps the result Unknown. Confirm actionable results across polls before notifying (30-second default).

Do not infer participation solely from a cached Skipped result when that reviewer's activity disappears. Fresh activity reintroduces the reviewer; previously participating reviewers with missing results and explicitly required reviewers still block. Log when a skipped reviewer drops out, and identify historical participation in missing-evidence reasons.

Keep reviewer detection in separate modules. Treat these observed conventions as evidence to validate, not guaranteed API contracts:

- **Cubic:** A running PR check indicates review activity. Review summaries include `N issues found`, `No issues found`, or `0 issues found`; summaries may be edited after findings are addressed.
- **CodeRabbit:** Uses checks or legacy commit statuses for activity. Require an explicit current-commit review verdict; successful or skipped statuses alone are not approval.
- **Codex:** Uses an `eyes` reaction on the PR description while reviewing, often with an edited `Codex Review Summary` comment. It may remove `eyes` when posting findings or add `+1` for approval. Attribute reactions to the agent; reactions alone do not identify the reviewed commit.

## Architecture

- Rust; native AppKit popover via `objc2`, tray and alternative menus via `tray-icon`/`muda`, background work via `tokio`, parsing via `serde`.
- Use authenticated `gh api graphql` and REST calls. Require an installed, authenticated GitHub CLI; support common install locations and a configured executable path.
- Poll tracked PRs about every 30 seconds; discovery may be less frequent. Batch final PR head/account checks, paginate fully, apply timeouts/backoff, and inspect edited summaries and reactions directly.
- Discover authored PRs through the direct `viewer.pullRequests` connection; use search for assignments and review requests. Search omissions must not evict tracked PRs or reset their acknowledgements. For the same authenticated account, keep polling known PRs until direct evidence confirms closure or the user ignores them.
- Share bounded REST ETag caches and quota cooldowns across clients using a local credential fingerprint, with separate core/GraphQL quotas and secondary limits shared across resources for that credential. Resolve credentials with local gh auth token before applying a remote quota; never log tokens or persist them. Probe credentials during cooldowns so account switches resume polling, preserve old quota deadlines for switching back, and pin mutation requests to their final validated client. Allow label-refresh authentication through REST when only GraphQL is exhausted. Handle `gh` exiting nonzero for valid 304 responses. Respect Retry-After and primary reset headers; secondary limits without guidance wait at least 60 seconds with exponential backoff. Manual requests must not bypass cooldowns, and mutations must not be automatically retried. Log quota metadata without tokens or response bodies.
- Run one background `.app` in the user session, with launch at login through `SMAppService`. Use native UserNotifications through `objc2` for notification clicks. Keep polling and disk work off the UI thread.
- Show missing-CLI/authentication errors in both the menu and notifications, deduplicating repeated failures. Distinguish network failures. Request notification permission on first launch.

## Persistence and diagnostics

- Store Gopher data under `~/.config/gopher`; do not use macOS Application Support defaults. Sparkle is an explicit exception: let it own updater preferences in NSUserDefaults and temporary downloads in macOS caches.
- Use `state.sqlite3` via `rusqlite` for cached PR data, reviewer evidence/runs, update identifiers, acknowledgements, ignored PR IDs, and notification history. GitHub remains authoritative; show restored data as stale until refreshed.
- Keep optional settings in `config.toml`.
- Write structured JSONL logs to `logs/gopher.jsonl` using `tracing`, retaining at most **10,000 lines across retained logs**. Bound individual entry sizes and safely replace files when trimming.
- Log request timing/failures, detection evidence, state transitions, notifications, and acknowledgements with timestamps, severity, and PR/reviewer identifiers. Exclude credentials and full API responses by default. Log allowlisted request-failure reasons without raw CLI stderr; distinguish TLS handshake timeouts from certificate verification failures and Gopher process timeouts. Timeout messages must not suggest a certificate failure.
- Keep lifecycle diagnostics independent of log filters, with bounded waits for durable writes. Record Rust panics and orderly quits/signals. Maintain `session.json` only under the app instance lock, completing it explicitly after shutdown; report incomplete previous sessions without inventing a cause. CLI diagnostics must not alter it. Test crashes and signals in isolated subprocesses.

## Installation and shutdown

`scripts/install.sh` is a standalone macOS Bash entry point, distributed as a release asset. Require authenticated `gh`, resolve one stable `v<version>` release, and verify its checksum, archive paths, app version, and Apple signature against the Gopher identifier and signing team before running its bundled installation helper. Do not add Python/Xcode requirements or weaken Gatekeeper. Install only into `~/Applications`; preserve `~/.config/gopher` and login preferences. Reject downgrades, unknown signatures, symlink destinations, and concurrent installers; leave equal versions unchanged. The verified Rust helper uses an installer lock and the existing app instance lock during replacement, stages before shutdown, and restores old files on replacement failure. Open fresh installs; preserve running/stopped state for upgrades; support `--no-launch`. Never force-kill an app that cannot finish shutdown. Older running apps without `GopherInstallerProtocol=1` need a manual quit.

Quit/SIGTERM cancels unsubmitted merges/labels and stops accepting actions, then drains submitted mutations and persists queued label results before dropping the runtime/instance lock. Track submitted requests independently of displayed intents, which polls or account changes can invalidate. Never retry a mutation during shutdown. Test installers with simulated downloads and mutations, isolated directories, and no published test release. Leave Gopher stopped after Mac Mini testing.

## In-app updates

Release bundles embed checksum-pinned Sparkle 2 and its helpers; normal local builds and CLI commands never start an updater. Keep the framework self-contained and preserve symlinks/executable permissions. Sign nested code inside-out, then the host. Require signed archives before extraction and signed feeds with no expiry fallback, using the committed Ed25519 public key. Private keys belong in Keychain or protected CI secrets, never Git/logs/command-line arguments. Release feeds use immutable versioned archive URLs. Publishing is exclusively through the manually dispatched Release workflow on main; validation-only is the default. Keep the dispatched SHA pinned, require a newer stable Cargo version and a matching nonempty changelog entry, and publish only after all draft assets match the signed build. Recover only workflow-owned drafts for the same SHA/version; never overwrite published assets/tags. Signing secrets belong to the main-only release environment. See docs/updates.md for recovery and first-release rules. The repository release skill requires a version choice and approval of the changelog before commit/push/publication, prefers direct main pushes when allowed, and always dispatches with the approved full expected_sha. Preserve preparation-only scope and the GOP-7 first-publication gate.

Default to daily automatic checks/downloads and installation on normal quit; never invoke an unsolicited restart. Put one Check for updates… entry immediately above Quit Gopher, changing it to Update available… when appropriate. Use Sparkle's standard dialog on explicit invocation and gentle menu-only reminders otherwise; no update notification or changed tray icon. Settings exposes immediate check/download preferences and version/status. Keep Sparkle's preferences authoritative, including changes made from its own dialog.

Intercept native termination without replacing Tao's other delegate behavior. Cancel the first native quit request, drain the worker asynchronously, persist queued labels and finish lifecycle/log cleanup, then issue native termination again. Do not use NSTerminateLater: its modal loop prevents Tao from unwinding. The shell installer must refuse overlap with Sparkle's separate installation process, checking before and after host shutdown. Exercise real signed updates with an isolated app identity/data directory and fake GitHub access; `examples/update_smoke.rs` covers explicit relaunch with delayed simulated persistence. Leave Mini test apps/helpers stopped.

## Development guidance

Prefer structural symbol navigation for unfamiliar code when available; use text search for strings and cross-file usages. Test reviewer parsing and state transitions with representative fixtures, especially stale approvals, edited summaries, overlapping runs, and acknowledgement races.

- Check: `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`.
- Diagnose: `cargo run -- doctor`, `cargo run -- inspect OWNER/REPO NUMBER` (read-only; no notifications).
- Package: `sh scripts/bundle.sh` builds explicitly for Apple Silicon/macOS 13. Add `--release` for a versioned unnotarized ZIP and SHA-256 checksum; release packaging requires a real signing identity, while local builds may fall back to ad-hoc signing. Prefer an installed Apple Development identity; use `GOPHER_SIGNING_IDENTITY` to override. Generate bundle versions from Cargo metadata before signing, with build version `(major + 1).minor.patch`. Stage fresh bundles and verify the extracted ZIP before replacing outputs. Run packaging tests with `python3 -m unittest discover -s scripts -p 'test_*.py'`. Notifications and login startup must also be tested on another Mac; an SSH copy does not test browser-download Gatekeeper behavior.

## Git

Use Conventional Commits: `type(scope): description`, with an optional scope. Choose a descriptive type such as `feat`, `fix`, `docs`, `refactor`, `test`, or `chore`, and keep the subject concise and imperative.

## Linear

This project uses Linear for issue tracking. Issues are in the `Gopher` team. When given a Linear issue for feature work, move it to "In Progress", assign it to the current user if possible, and checkout its git branch. You may make these changes even in no-edit mode.
