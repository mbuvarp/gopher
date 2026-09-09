# Gopher

A native Rust macOS menu bar inbox for GitHub agent reviews. Tracks open PRs you authored, are assigned to, or are requested to review, grouped by repository under `repo • organization` headings, ordered by organization, repository, then ascending PR number.

## Experimental popover

On `feature/custom-popover`, **left-click** Gopher to open a native, scrollable review inbox. Unacknowledged updates have bold PR titles. Click a title to acknowledge without closing the popover; it returns to regular weight after the change is saved. **Open PR** also acknowledges after opening the browser. **Open PR**, **Details**, and **Ignore** are grouped on the left of each row. **Details** expands reviewer evidence, and **Ignore** hides the PR until restored. The header includes Refresh and an Actions menu. Its first item, **Show ignored**, opens the ignored PR list with **Restore** buttons and a **Back** button in the header. Restoring resumes discovery and monitoring for eligible open PRs. Actions also contains configuration, logs, notifications, launch at login, and Quit.

The popover updates existing rows while open, retaining expanded details and scroll position. Click outside to dismiss it. **Right-click** the menu bar icon to use the original menu for comparison. Both interfaces share the same database and notification behavior. Ignored PR details are cached separately. A lightweight check runs at startup and every 15 minutes, hiding closed or merged PRs from this view. Their ignore flags remain saved: if reopened, they reappear here and stay ignored. Failed lookups leave the last known status intact; unavailable entries remain restorable even while offline.

## Run

Requires macOS 13+, Rust, Xcode command-line tools, and an installed GitHub CLI authenticated with access to your repositories:

```sh
gh auth login
cargo run -- doctor
sh scripts/bundle.sh
mkdir -p ~/Applications
cp -R dist/Gopher.app ~/Applications/
open ~/Applications/Gopher.app
```

Allow notifications when prompted. Enable **Launch at login** in Gopher's menu to start it automatically in your user session. Quit Gopher before replacing an installed build. The build script uses an installed Apple Development signing identity, or `GOPHER_SIGNING_IDENTITY` if specified. Without one it falls back to ad-hoc signing and warns that notification authorization may fail. Distributing to other machines requires appropriate signing/notarization.

`cargo run` also starts the menu, but notifications and launch at login require the bundled app. No web frontend, server, webhook setup, or separate system daemon is needed.

## Review behavior

- **? Unknown:** No reliable current-commit result, missing reviewer evidence, or stale cached data.
- **◌ Reviewing:** At least one participating agent is running, or a final result is being confirmed across polls.
- **● Comments:** All participating agents have finished and unresolved review threads remain.
- **✓ Approved:** All participating agents have an explicit clean result for the current changes, with no unresolved threads.

Findings are held until every participating reviewer finishes. Completed results must remain stable for 30 seconds by default, avoiding notifications while an agent is still publishing its output. An approval describes agent review only; it is not a guarantee of CI success, human approval, or mergeability.

Each PR has **Open PR**, **Acknowledge update**, and **Ignore PR** actions. Clicking the PR row itself acknowledges the displayed update; hovering opens its submenu. Opening a PR successfully acknowledges its displayed update and clears that update’s notification. **Ignore PR** removes that PR from review polling, the active list, and notifications, including after restarts or account changes, until you restore it through **Show ignored**. Unacknowledged results affect the main icon, with comments taking priority over approvals. The plain cartoon gopher is used both when nothing needs attention and while reviews are running or being confirmed; background polling alone does not change the icon. The question mark is reserved for errors or unacknowledged unknown/stale PRs. Clicking a review notification acknowledges its update without opening the browser. Its action menu offers **Acknowledge** and **Open PR**; **Open PR** opens the browser and acknowledges after it opens successfully. Older notifications cannot acknowledge newer updates.

Codex detection uses its bot reactions, commit-specific reviews, and persistent review summary. Cubic uses checks and explicit issue counts. CodeRabbit uses checks and explicit review verdicts. Successful checks alone do not establish approval. Unknown formats and reactions without a reliable commit association stay Unknown. Resolved threads alone do not establish a clean review.

Reviewers are inferred from activity on each PR, including previously observed agents. Explicit subscription-limit or paused-review notices are shown as Skipped and excluded from automatic participation; an explicitly required reviewer that skips keeps the result Unknown. If a subscription or repository setup changes, use an explicit reviewer list. Polling can miss a complete rerun between polls, particularly one represented only by reactions; ambiguous evidence is intentionally conservative.

## Configuration and data

Everything is stored in `~/.config/gopher`:

```text
config.toml           Optional settings; restart to apply
state.sqlite3         Cached PRs, review evidence, acknowledgements, ignored PRs, notification history
logs/gopher.jsonl     Structured logs, at most 10,000 retained lines
gopher.lock           Prevents concurrent app instances
```

SQLite may also create `state.sqlite3-wal` and `state.sqlite3-shm`. GitHub remains authoritative. Restored data stays marked cached until successfully fetched. Changing the active GitHub account resets cached state and acknowledgements.

Use **Edit configuration** to create/open a configuration file, or copy [config.example.toml](config.example.toml). Settings include polling/discovery intervals, request timeout, result confirmation interval, logging verbosity, `gh_path`, notifications, and repository overrides:

```toml
[repositories."owner/repo"]
reviewers = ["codex", "cubic"]
# ignore = true
```

Polling uses `gh api`, fully paginates PR connections and checks, and checks that the head commit did not change during a fetch. Up to three PRs are fetched concurrently. Errors trigger bounded exponential backoff; **Refresh now** retries immediately. Authentication and network errors remain visible in the menu and produce a notification when the error changes.

Logs include request timing at `debug`, reviewer evidence, state transitions, notification scheduling, and acknowledgement events. Failed requests include the request type, HTTP status when available, CLI exit code, and a classified cause; PR fetch failures include the repository and PR number. Tokens and full API responses are not logged. Individual records are limited to 16 KiB; oversized records are replaced with a diagnostic entry. The asynchronous log queue is bounded; overload drops entries with a stderr diagnostic. The SQLite cache does contain private PR content and should be treated accordingly.

## Diagnose and develop

```sh
cargo run -- doctor
cargo run -- inspect owner/repo 123
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`doctor` verifies configuration, CLI discovery, and authentication. `inspect` fetches live evidence and prints a JSON verdict without writing PR state or sending notifications. It does not have the running app's historical evidence and does not apply the confirmation interval.

See [AGENTS.md](AGENTS.md) for project conventions. Use Conventional Commits.
