# Gopher

A native Rust macOS menu bar inbox for GitHub agent reviews. Tracks open PRs you authored, are assigned to, or are requested to review, grouped by repository under `repo • organization` headings, ordered by organization, repository, then ascending PR number.

## Using Gopher

**Left-click** the gopher in the menu bar to open a native, scrollable review inbox. Each PR has a status icon, title, review status, and unresolved-thread count, followed by compact pills in its GitHub label colors. Labels use the detail row’s font size, wrap when needed, and refresh during normal polling or after a successful label change. Unacknowledged actionable updates have **bold blue titles** and blue status icons. Click a title to acknowledge its displayed update without closing the popover; its title returns to regular weight and both title and icon return to their normal colors after the change is saved.

The buttons beneath each PR let you:

- **Open PR** in the browser and acknowledge the displayed update after it opens successfully.
- Expand **Details** to inspect reviewer evidence.
- Open the right-aligned **Actions** dropdown for repository configuration, enabled PR actions, and **Ignore** at the bottom. Ignore hides the PR from the active list, review polling, and notifications until restored.

The header has **Refresh** and an **Actions** menu. Refresh is disabled and reads **Refreshing...** while PRs are being fetched. Actions contains **Show ignored**, configuration, notification settings, logs, launch at login, and Quit Gopher. **Show ignored** replaces the active list with ignored PRs, showing **Restore** buttons and a **Back** button in the header. Restoring resumes discovery for eligible open PRs.

The popover updates while open, retaining expanded details and each view's scroll position. Click outside to dismiss it. **Right-click** the menu bar icon for the alternative standard menu, where PR submenus include an **Acknowledge update** checkbox. Both interfaces share the same state and notification behavior.

Ignored PRs are checked at startup and every 15 minutes without fetching reviews. Closed or merged PRs disappear from the ignored list, but keep their ignore flags: if reopened, they reappear there and remain ignored. Unavailable identities keep their cached state and do not block updates to accessible PRs. They can still be restored while offline.

## PR actions

Choose **Actions → Configure** on any active PR to configure actions for its repository. The table has **Action**, **Enabled**, **Condition**, and **Merge method** columns. Changes save immediately to SQLite and apply to every PR in that repository, without restarting. Both actions start disabled; Merge defaults to the Approved condition and Merge commit method, while Label defaults to Always.

Conditions are **Always**, **Reviewing**, **Comments**, and **Approved**. Enabled actions remain visible but disabled when their condition is not met or PR data is stale. Draft PRs cannot be merged.

- **Merge** offers Merge commit, Squash, or Rebase. Clicking it replaces the dropdown with **Cancel (5s)**, counting down before attempting the merge. Closing the menu/popover or navigating elsewhere does not cancel it; quitting Gopher does. Gopher rechecks the PR and targets the selected commit. GitHub blockers appear beneath the PR; Gopher does not enable auto-merge, join a merge queue, bypass protection, or retry automatically.
- **Label** opens a persistent picker with colored dots and checked labels. Toggle several labels without leaving the panel; each change adds or removes only that label. **Refresh labels** reloads the available and assigned labels, and **Back** returns to the PR list.

These actions use the authenticated GitHub CLI account and require its normal repository permissions. They are available in the popover; the alternative standard menu retains its existing review controls.

## Run

Requires macOS 13+, Rust, Xcode command-line tools, and an installed GitHub CLI authenticated with access to your repositories:

```sh
gh auth login
cargo run -- doctor
sh scripts/bundle.sh
mkdir -p ~/Applications
ditto dist/Gopher.app ~/Applications/Gopher.app
open ~/Applications/Gopher.app
```

Allow notifications when prompted. Enable **Actions → Launch at login** to start Gopher automatically in your user session. Quit Gopher before replacing an installed build. The build script packages the application icon and uses an installed Apple Development signing identity, or `GOPHER_SIGNING_IDENTITY` if specified. Without one it falls back to ad-hoc signing and warns that notification authorization may fail. Distributing to other machines requires appropriate signing/notarization.

`cargo run` also starts the app, but notifications and launch at login require the bundled app. No web frontend, server, webhook setup, or separate system daemon is needed.

## Review behavior

- **Unknown:** No reliable current-commit result, missing reviewer evidence, or stale cached data.
- **Reviewing:** At least one participating agent is running, or a final result is being confirmed across polls.
- **Comments:** All participating agents have finished and unresolved review threads remain.
- **Approved:** All participating agents have an explicit clean result for the current changes, with no unresolved threads.

Reviewing includes elapsed time since Gopher observed the state, such as `Reviewing (7m)` or `Reviewing (1h 2m)`. Minutes are rounded down. The timer survives restarts and resets on a new commit or when the PR enters Reviewing again; it does not use GitHub's review start time.

Findings are held until every participating reviewer finishes. Completed results must remain stable for 30 seconds by default, avoiding notifications while an agent is still publishing its output. An approval describes agent review only; it is not a guarantee of CI success, human approval, or mergeability.

Unacknowledged results affect the menu bar icon, with comments taking priority over approvals. The plain cartoon gopher is used both when nothing needs attention and while reviews are running or being confirmed; background polling alone does not change the icon. The question mark is reserved for errors or unacknowledged unknown/stale PRs. Acknowledgement silences the displayed update and clears its notification; meaningful new updates need acknowledgement again.

Clicking a review notification acknowledges its update without opening the browser. Its action menu offers **Acknowledge** and **Open PR**; **Open PR** opens the browser and acknowledges after it opens successfully. Older notifications cannot acknowledge newer updates.

Codex detection uses its bot reactions, commit-specific reviews, and persistent review summary. Cubic uses checks and explicit issue counts. CodeRabbit uses checks and explicit review verdicts. Successful checks alone do not establish approval. Unknown formats and reactions without a reliable commit association stay Unknown. Resolved threads alone do not establish a clean review.

Reviewers are inferred from activity on each PR, including previously observed agents. Explicit subscription-limit or paused-review notices are shown as Skipped and excluded from automatic participation; an explicitly required reviewer that skips keeps the result Unknown. If a subscription or repository setup changes, use an explicit reviewer list. Polling can miss a complete rerun between polls, particularly one represented only by reactions; ambiguous evidence is intentionally conservative.

## Configuration and data

Everything is stored in `~/.config/gopher`:

```text
config.toml           Optional settings; restart to apply
state.sqlite3         PR cache, review evidence, acknowledgements, ignored PRs, action settings, notification history
logs/gopher.jsonl     Structured logs, at most 10,000 retained lines
gopher.lock           Prevents concurrent app instances
```

SQLite may also create `state.sqlite3-wal` and `state.sqlite3-shm`. GitHub remains authoritative. Cached data stays marked stale until successfully fetched. Changing the active GitHub account resets the active cache and acknowledgements; ignored PR flags, their archived details, and repository action settings persist across account changes. Pending actions are not restored after restart.

Use **Edit configuration** to create/open a configuration file, or copy [config.example.toml](config.example.toml). Settings include polling/discovery intervals, request timeout, result confirmation interval, logging verbosity, `gh_path`, notifications, and repository overrides:

```toml
[repositories."owner/repo"]
reviewers = ["codex", "cubic"]
# ignore = true
```

Gopher polls active PRs every 30 seconds and discovers new PRs every two minutes by default. Authored PRs come from GitHub's direct API; assignments and review requests use search. Search omissions do not remove known PRs or erase acknowledgements: for the same account, Gopher keeps monitoring a tracked PR until direct evidence confirms closure or you ignore it.

Polling uses `gh api`, fully paginates PR connections and checks, and checks that the head commit did not change during a fetch. Up to three PRs are fetched concurrently. Errors trigger bounded exponential backoff; **Refresh** retries immediately. Authentication and network errors remain visible in the inbox and produce a notification when the error changes.

Logs include request timing at `debug`, reviewer evidence, state transitions, notification scheduling, and acknowledgement events. Failed requests include the request type, HTTP status when available, CLI exit code, and a classified cause; PR fetch failures include the repository and PR number. Tokens and full API responses are not logged. Individual records are limited to 16 KiB; oversized records are replaced with a diagnostic entry. The asynchronous log queue is bounded; overload drops entries with a stderr diagnostic. The SQLite cache does contain private PR content and should be treated accordingly.

## Diagnose and develop

```sh
cargo run -- doctor
cargo run -- inspect owner/repo 123
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

`doctor` verifies configuration, CLI discovery, and authentication. `inspect` fetches live evidence and prints a JSON verdict without writing PR state or sending notifications. It does not have the running app's historical evidence and does not apply the confirmation interval.

See [AGENTS.md](AGENTS.md) for project conventions. Use Conventional Commits.
