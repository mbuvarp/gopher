use super::*;
use crate::worker::transition;
use std::{os::unix::fs::PermissionsExt, sync::Arc};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

struct Harness {
    directory: tempfile::TempDir,
    store: Store,
    coordinator: Coordinator,
    prs: BTreeMap<String, PullRequest>,
    config: Config,
    sender: UnboundedSender<Command>,
    receiver: UnboundedReceiver<Command>,
    viewer: &'static str,
    sink: Sink,
}

#[tokio::test]
async fn shutdown_cancels_countdown_and_rejects_new_actions() {
    let mut h = Harness::new();
    let token = h.start_merge();
    h.coordinator.begin_shutdown();
    h.handle(ActionCommand::Tick {
        pr: "PR_1".into(),
        token,
        remaining: 0,
    });
    h.request(Request::Merge {
        on_green: false,
        pr: "PR_1".into(),
        head: "head".into(),
        update: h.prs["PR_1"].update_id.clone(),
    });
    assert!(h.coordinator.merges.is_empty());
    assert!(!h.coordinator.has_submissions());
    h.assert_no_merge();
}

#[tokio::test]
async fn shutdown_waits_for_a_submitted_merge_even_after_intent_invalidation() {
    let mut h = Harness::new();
    h.refresh_pr().await;
    let token = h.start_merge();
    h.handle(ActionCommand::Tick {
        pr: "PR_1".into(),
        token,
        remaining: 0,
    });
    while !h.coordinator.has_submissions() {
        h.step().await;
    }
    // UI/account reconciliation may already have discarded the displayed intent.
    h.coordinator.merges.clear();
    h.coordinator.begin_shutdown();
    assert!(h.coordinator.has_submissions());
    while h.coordinator.has_submissions() {
        h.step().await;
    }
    assert!(h.directory.path().join("merge.json").exists());
    // The worker still learns the PR merged, so it is never polled back in.
    assert_eq!(h.coordinator.take_merged(), ["PR_1"]);
}

#[tokio::test]
async fn shutdown_finishes_submitted_label_but_cancels_the_queue() {
    let mut h = Harness::new();
    h.coordinator.state.labels.insert(
        "PR_1".into(),
        Labels {
            items: ["one", "two"]
                .into_iter()
                .map(|name| Label {
                    name: name.into(),
                    color: "ff0000".into(),
                    selected: false,
                })
                .collect(),
            ..Default::default()
        },
    );
    for name in ["one", "two"] {
        h.request(Request::Label {
            pr: "PR_1".into(),
            name: name.into(),
            selected: true,
        });
    }
    h.request(Request::SaveLabels {
        pr: "PR_1".into(),
        name: None,
    });
    while !h.coordinator.has_submissions() {
        h.step().await;
    }
    h.coordinator.begin_shutdown();
    assert!(h.coordinator.has_submissions());
    while h.coordinator.has_submissions() {
        h.step().await;
    }
    let mutations = std::fs::read_to_string(h.directory.path().join("labels.log")).unwrap();
    assert_eq!(mutations.lines().count(), 1);
    assert!(matches!(
        h.receiver.try_recv().unwrap(),
        Command::LabelSaved { selected: true, .. }
    ));
    assert!(h.coordinator.label_jobs.values().all(VecDeque::is_empty));
}
#[tokio::test]
async fn shutdown_reports_submitted_label_after_account_invalidation() {
    let mut h = Harness::new();
    h.coordinator.state.labels.insert(
        "PR_1".into(),
        Labels {
            items: vec![Label {
                name: "one".into(),
                color: "ff0000".into(),
                selected: false,
            }],
            ..Default::default()
        },
    );
    h.request(Request::Label {
        pr: "PR_1".into(),
        name: "one".into(),
        selected: true,
    });
    h.request(Request::SaveLabels {
        pr: "PR_1".into(),
        name: None,
    });
    while !h.coordinator.has_submissions() {
        h.step().await;
    }
    h.viewer = "another-account";
    h.store.set_viewer(h.viewer).unwrap();
    h.coordinator.reconcile(&Context {
        store: &h.store,
        prs: &h.prs,
        viewer: Some(h.viewer),
        config: &h.config,
        sender: &h.sender,
        sink: &h.sink,
    });
    assert!(h.coordinator.label_jobs.is_empty());
    // Even a new picker for the same PR must not be changed by the old result.
    h.coordinator.state.labels.insert(
        "PR_1".into(),
        Labels {
            items: vec![Label {
                name: "one".into(),
                color: "112233".into(),
                selected: false,
            }],
            pending: ["one".into()].into(),
            ..Default::default()
        },
    );
    h.coordinator.begin_shutdown();
    while h.coordinator.has_submissions() {
        h.step().await;
    }
    match h.receiver.try_recv().unwrap() {
        Command::LabelSaved {
            viewer,
            label,
            selected,
            ..
        } => {
            assert_eq!(viewer, "test");
            assert_eq!(label.name, "one");
            assert_eq!(label.color, "ff0000");
            assert!(selected);
        }
        _ => panic!("Missing submitted label result"),
    }
    assert!(h.coordinator.submitted_labels.is_empty());
    assert!(h.coordinator.completed_labels.is_empty());
    let labels = &h.coordinator.state.labels["PR_1"];
    assert!(!labels.items[0].selected);
    assert!(labels.pending.contains("one"));
}

impl Harness {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let gh = directory.path().join("gh");
        let fixture = include_str!("../../../tests/fixtures/gh-snapshot.sh").replace(
            "input=$(cat)",
            r#"
input=$(cat)
case "$input" in
 *'mutation ReadyForReview'*)
   printf '%s' "$input" > "$(dirname "$0")/ready.json"
   echo '{"data":{"markPullRequestReadyForReview":{"pullRequest":{"id":"PR_1","isDraft":false}}}}'
   exit 0;;
esac
case "$input" in *viewer*) echo '{"data":{"viewer":{"login":"test"}}}'; exit 0;; esac
"#,
        );
        std::fs::write(&gh, format!(r#"#!/bin/sh
if [ "$1" = auth ]; then echo test-credential; exit 0; fi
case "$*" in
 *'--method PUT'*) printf '%s\n' "$*" >> "$(dirname "$0")/merges.log"; cat > "$(dirname "$0")/merge.json"; echo '{{"merged":true}}'; exit 0;;
 *'--method POST'*|*'--method DELETE'*) printf '%s\n' "$*" >> "$(dirname "$0")/labels.log"; cat >/dev/null; echo '[]'; exit 0;;
esac
{fixture}
"#)).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut store = Store::open(directory.path()).unwrap();
        store.set_viewer("test").unwrap();
        let preferences = Preferences {
            merge: Rule {
                enabled: true,
                condition: Condition::Always,
            },
            label: Rule {
                enabled: true,
                condition: Condition::Always,
            },
            ..Default::default()
        };
        store
            .save_action_preferences("owner/repo", &preferences)
            .unwrap();
        let pr = transition(
            Snapshot {
                id: "PR_1".into(),
                repo: "owner/repo".into(),
                number: 1,
                head: "head".into(),
                open: true,
                ..Default::default()
            },
            None,
            None,
            100,
            0,
        );
        let coordinator = Coordinator::new(&store).unwrap();
        let (sender, receiver) = unbounded_channel();
        Self {
            directory,
            store,
            coordinator,
            prs: BTreeMap::from([("PR_1".into(), pr)]),
            config: Config {
                gh_path: Some(gh),
                settle_seconds: 0,
                ..Default::default()
            },
            sender,
            receiver,
            viewer: "test",
            sink: Arc::new(|_| {}),
        }
    }
    fn handle(&mut self, command: ActionCommand) {
        self.coordinator.handle(
            command,
            &Context {
                store: &self.store,
                prs: &self.prs,
                viewer: Some(self.viewer),
                config: &self.config,
                sender: &self.sender,
                sink: &self.sink,
            },
        );
    }
    fn request(&mut self, request: Request) {
        self.handle(ActionCommand::Request(request));
    }
    fn ready_request(&self) -> Request {
        let pr = &self.prs["PR_1"];
        Request::ReadyForReview {
            pr: pr.snapshot.id.clone(),
            head: pr.snapshot.head.clone(),
            update: pr.update_id.clone(),
        }
    }
    fn start_merge(&mut self) -> u64 {
        self.merge("PR_1")
    }
    fn merge(&mut self, pr: &str) -> u64 {
        self.request(Request::Merge {
            on_green: false,
            pr: pr.into(),
            head: "head".into(),
            update: self.prs[pr].update_id.clone(),
        });
        self.coordinator.merges[pr].token
    }
    /// Ends the countdown immediately instead of waiting five seconds.
    fn finish_countdown(&mut self, pr: &str) {
        let token = self.coordinator.merges[pr].token;
        self.handle(ActionCommand::Tick {
            pr: pr.into(),
            token,
            remaining: 0,
        });
    }
    fn patch_gh(&self, from: &str, to: &str) {
        let gh = self.config.gh_path.as_ref().unwrap();
        let script = std::fs::read_to_string(gh).unwrap();
        assert!(script.contains(from));
        std::fs::write(gh, script.replacen(from, to, 1)).unwrap();
    }
    fn merge_log(&self) -> Vec<String> {
        std::fs::read_to_string(self.directory.path().join("merges.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
    fn progress(&self, pr: &str) -> Option<&MergeProgress> {
        self.coordinator.state.merges.get(pr)
    }
    fn reconcile(&mut self) {
        self.coordinator.reconcile(&Context {
            store: &self.store,
            prs: &self.prs,
            viewer: Some(self.viewer),
            config: &self.config,
            sender: &self.sender,
            sink: &self.sink,
        });
    }
    async fn step(&mut self) {
        let command = tokio::time::timeout(Duration::from_secs(8), self.receiver.recv())
            .await
            .unwrap()
            .unwrap();
        if let Command::PrAction(command) = command {
            self.handle(command);
        }
    }
    /// Handles action commands until the coordinator reports a saved ready result.
    async fn ready_saved(&mut self) -> (String, String) {
        loop {
            let command = tokio::time::timeout(Duration::from_secs(8), self.receiver.recv())
                .await
                .unwrap()
                .unwrap();
            match command {
                Command::PrAction(action) => self.handle(action),
                Command::ReadySaved { pr, viewer } => break (pr, viewer),
                _ => {}
            }
        }
    }
    fn finish_ready(&mut self, pr: &str) {
        self.coordinator.ready_saved(
            pr,
            &Context {
                store: &self.store,
                prs: &self.prs,
                viewer: Some(self.viewer),
                config: &self.config,
                sender: &self.sender,
                sink: &self.sink,
            },
        );
    }
    async fn refresh_pr(&mut self) {
        self.add_pr("PR_1", 1).await;
    }
    async fn add_pr(&mut self, id: &str, number: u64) {
        let snapshot = Github::new(&self.config)
            .unwrap()
            .snapshot(&PrRef {
                id: id.into(),
                repo: "owner/repo".into(),
                number,
            })
            .await
            .unwrap();
        self.prs
            .insert(id.into(), transition(snapshot, None, None, 100, 0));
    }
    fn assert_no_merge(&self) {
        assert!(!self.directory.path().join("merge.json").exists());
    }
}

#[tokio::test]
async fn ready_for_review_submits_for_draft_without_check_requirements() {
    let mut h = Harness::new();
    let gh = h.config.gh_path.as_ref().unwrap();
    let script = std::fs::read_to_string(gh).unwrap().replace(
        "\"state\":\"OPEN\",\"isDraft\":false",
        "\"state\":\"OPEN\",\"isDraft\":true",
    );
    std::fs::write(gh, script).unwrap();
    h.refresh_pr().await;
    assert!(h.prs["PR_1"].snapshot.draft);
    h.request(h.ready_request());
    // Displayed as ready from the click, before GitHub has been contacted.
    assert!(h.coordinator.state.ready_pending("PR_1"));
    let saved = h.ready_saved().await;
    assert_eq!(saved, ("PR_1".into(), "test".into()));
    assert!(h.directory.path().join("ready.json").exists());
    // Still displayed as ready until the worker applies ReadySaved, so the
    // row does not flash back to draft in between.
    assert!(h.coordinator.state.ready_pending("PR_1"));
    h.finish_ready("PR_1");
    assert!(!h.coordinator.state.ready.contains_key("PR_1"));
}

#[tokio::test]
async fn ready_for_review_rejects_stale_prs() {
    let mut h = Harness::new();
    let pr = h.prs.get_mut("PR_1").unwrap();
    pr.snapshot.draft = true;
    pr.stale = true;
    h.request(h.ready_request());
    assert!(h.coordinator.ready.is_empty());
    assert!(!h.coordinator.state.ready_pending("PR_1"));
    assert!(!h.directory.path().join("ready.json").exists());
}

#[tokio::test]
async fn ready_for_review_accepts_a_pr_already_ready_on_github() {
    let mut h = Harness::new();
    // Locally a draft, but GitHub's snapshot already reports it as ready.
    let pr = h.prs.get_mut("PR_1").unwrap();
    pr.snapshot.draft = true;
    h.request(h.ready_request());
    let saved = h.ready_saved().await;
    assert_eq!(saved, ("PR_1".into(), "test".into()));
    assert!(h.coordinator.state.ready_pending("PR_1"));
    assert!(h.coordinator.state.error.is_none());
    assert!(!h.coordinator.has_submissions());
    assert!(!h.directory.path().join("ready.json").exists());
}

#[tokio::test]
async fn ready_saved_keeps_a_newer_request_pending() {
    let mut h = Harness::new();
    h.prs.get_mut("PR_1").unwrap().snapshot.draft = true;
    h.request(h.ready_request());
    // A ReadySaved for an earlier request must not end this one's display.
    h.finish_ready("PR_1");
    assert!(h.coordinator.state.ready_pending("PR_1"));
}

#[tokio::test]
async fn ready_for_review_rejects_an_update_that_changed_while_menu_was_open() {
    let mut h = Harness::new();
    h.refresh_pr().await;
    h.prs.get_mut("PR_1").unwrap().snapshot.draft = true;
    let displayed = h.ready_request();
    h.prs.get_mut("PR_1").unwrap().update_id = "new-review-update".into();
    h.request(displayed);
    assert!(h.coordinator.ready.is_empty());
    assert!(
        h.coordinator
            .state
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("review changed")
    );
    assert!(!h.directory.path().join("ready.json").exists());
}

#[tokio::test]
async fn ready_for_review_rejects_head_changes_during_validation() {
    let mut h = Harness::new();
    let gh = h.config.gh_path.as_ref().unwrap();
    let script = std::fs::read_to_string(gh).unwrap().replace(
        "\"state\":\"OPEN\",\"isDraft\":false",
        "\"state\":\"OPEN\",\"isDraft\":true",
    );
    std::fs::write(gh, script).unwrap();
    h.refresh_pr().await;
    h.request(h.ready_request());
    assert!(h.coordinator.state.ready_pending("PR_1"));
    h.prs.get_mut("PR_1").unwrap().snapshot.head = "new-head".into();
    h.step().await;
    // The optimistic display reverts and the failure is shown instead.
    assert!(!h.coordinator.state.ready_pending("PR_1"));
    assert!(matches!(
        h.coordinator.state.ready.get("PR_1"),
        Some(ReadyProgress::Failed(_))
    ));
    assert!(!h.directory.path().join("ready.json").exists());
}

#[test]
fn ready_failure_clears_when_another_actor_marks_the_pr_ready() {
    let mut h = Harness::new();
    h.coordinator.state.ready.insert(
        "PR_1".into(),
        ReadyProgress::Failed(failure(1, "Ready for review cancelled")),
    );
    h.coordinator.reconcile(&Context {
        store: &h.store,
        prs: &h.prs,
        viewer: Some(h.viewer),
        config: &h.config,
        sender: &h.sender,
        sink: &h.sink,
    });
    assert!(!h.coordinator.state.ready.contains_key("PR_1"));
}

fn failure(id: u64, message: &str) -> Failure {
    Failure {
        id,
        message: message.into(),
    }
}

#[test]
fn dismissing_an_error_removes_only_the_displayed_failure() {
    let mut h = Harness::new();
    // Ready failures are only kept for drafts.
    h.prs.get_mut("PR_1").unwrap().snapshot.draft = true;
    h.coordinator
        .state
        .ready
        .insert("PR_1".into(), ReadyProgress::Failed(failure(1, "failed")));
    h.coordinator
        .state
        .merges
        .insert("PR_1".into(), MergeProgress::Failed(failure(2, "failed")));
    h.coordinator.state.error = Some(failure(3, "failed"));

    // Other occurrences, even with identical messages, are kept.
    for stale in [
        DisplayedError::Ready {
            pr: "PR_1".into(),
            id: 2,
        },
        DisplayedError::Merge {
            pr: "PR_1".into(),
            id: 1,
        },
        DisplayedError::Request(1),
    ] {
        h.request(Request::DismissError(stale));
    }
    assert!(h.coordinator.state.ready.contains_key("PR_1"));
    assert!(h.progress("PR_1").is_some());
    assert_eq!(h.coordinator.state.error, Some(failure(3, "failed")));

    h.request(Request::DismissError(DisplayedError::Ready {
        pr: "PR_1".into(),
        id: 1,
    }));
    assert!(!h.coordinator.state.ready.contains_key("PR_1"));
    assert_eq!(
        h.progress("PR_1"),
        Some(&MergeProgress::Failed(failure(2, "failed")))
    );
    assert_eq!(h.coordinator.state.error, Some(failure(3, "failed")));

    h.request(Request::DismissError(DisplayedError::Merge {
        pr: "PR_1".into(),
        id: 2,
    }));
    assert!(h.progress("PR_1").is_none());
    assert_eq!(h.coordinator.state.error, Some(failure(3, "failed")));

    h.request(Request::DismissError(DisplayedError::Request(3)));
    assert!(h.coordinator.state.error.is_none());
}

#[test]
fn repeated_failures_with_the_same_message_are_distinct() {
    let mut h = Harness::new();
    h.request(Request::LoadLabels("missing".into()));
    let first = h.coordinator.state.error.clone().unwrap();
    h.request(Request::LoadLabels("missing".into()));
    let second = h.coordinator.state.error.clone().unwrap();
    assert_eq!(first.message, second.message);
    assert_ne!(first.id, second.id);

    // A click bound to the first occurrence must not hide the second.
    h.request(Request::DismissError(DisplayedError::Request(first.id)));
    assert_eq!(h.coordinator.state.error, Some(second.clone()));
    h.request(Request::DismissError(DisplayedError::Request(second.id)));
    assert!(h.coordinator.state.error.is_none());
}

#[tokio::test]
async fn dismissing_an_old_merge_error_keeps_a_newer_pending_merge() {
    let mut h = Harness::new();
    h.coordinator
        .state
        .merges
        .insert("PR_1".into(), MergeProgress::Failed(failure(1, "failed")));
    let displayed = DisplayedError::Merge {
        pr: "PR_1".into(),
        id: 1,
    };
    let token = h.start_merge();
    h.request(Request::DismissError(displayed));
    assert_eq!(h.coordinator.merges["PR_1"].token, token);
    assert!(h.progress("PR_1").is_some_and(MergeProgress::busy));
    h.request(Request::CancelMerge("PR_1".into()));
}

#[tokio::test]
async fn merge_countdown_completes_without_ui_events_and_pins_the_commit() {
    let mut h = Harness::new();
    h.refresh_pr().await;
    let start = std::time::Instant::now();
    h.start_merge();
    h.assert_no_merge();
    assert_eq!(
        h.coordinator.state.merges["PR_1"],
        MergeProgress::Countdown(5)
    );
    for _ in 0..9 {
        h.step().await;
        if h.coordinator.state.merges["PR_1"] == MergeProgress::Complete {
            break;
        }
    }
    assert_eq!(h.coordinator.state.merges["PR_1"], MergeProgress::Complete);
    assert!(start.elapsed() >= Duration::from_secs(5));
    // The worker keeps the merged PR listed briefly, then removes it.
    assert_eq!(h.coordinator.take_merged(), ["PR_1"]);
    assert!(h.coordinator.take_merged().is_empty());
    h.reconcile();
    assert_eq!(h.progress("PR_1"), Some(&MergeProgress::Complete));
    h.prs.clear();
    h.reconcile();
    assert_eq!(h.progress("PR_1"), None);
    let payload: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(h.directory.path().join("merge.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(payload["sha"], "head");
    assert_eq!(payload["merge_method"], "merge");
}

#[tokio::test]
async fn cancellation_invalidates_old_ticks_and_in_flight_validation() {
    let mut h = Harness::new();
    let old = h.start_merge();
    h.request(Request::CancelMerge("PR_1".into()));
    let current = h.start_merge();
    h.handle(ActionCommand::Tick {
        pr: "PR_1".into(),
        token: old,
        remaining: 0,
    });
    assert_eq!(
        h.coordinator.state.merges["PR_1"],
        MergeProgress::Countdown(5)
    );
    h.handle(ActionCommand::Tick {
        pr: "PR_1".into(),
        token: current,
        remaining: 0,
    });
    assert_eq!(h.coordinator.state.merges["PR_1"], MergeProgress::Checking);
    h.request(Request::CancelMerge("PR_1".into()));
    h.step().await;
    assert!(h.coordinator.merges.is_empty());
    assert!(h.coordinator.state.merges.is_empty());
    h.assert_no_merge();
}

#[tokio::test]
async fn changed_commit_settings_account_ignore_or_conflicts_cancels_pending_merge() {
    for case in 0..5 {
        let mut h = Harness::new();
        let token = h.start_merge();
        match case {
            0 => h.prs.get_mut("PR_1").unwrap().snapshot.head = "new".into(),
            1 => h.request(Request::Configure {
                repo: "owner/repo".into(),
                change: Setting::MergeMethod(MergeMethod::Squash),
            }),
            2 => h.viewer = "different-user",
            3 => {
                h.prs.get_mut("PR_1").unwrap().snapshot.check_state =
                    Some(crate::model::CheckState::Conflicts)
            }
            _ => {
                h.prs.clear();
            }
        }
        h.handle(ActionCommand::Tick {
            pr: "PR_1".into(),
            token,
            remaining: 4,
        });
        assert!(h.coordinator.merges.is_empty());
        h.assert_no_merge();
    }
}

#[tokio::test]
async fn fresh_validation_rejects_changed_head_closed_draft_or_conflicting_pr() {
    for case in 0..4 {
        let mut h = Harness::new();
        let token = h.start_merge();
        let mut snapshot = h.prs["PR_1"].snapshot.clone();
        match case {
            0 => snapshot.head = "new-head".into(),
            1 => snapshot.open = false,
            2 => snapshot.draft = true,
            _ => snapshot.check_state = Some(crate::model::CheckState::Conflicts),
        }
        h.handle(ActionCommand::MergeChecked {
            pr: "PR_1".into(),
            token,
            result: Ok((Box::new(snapshot), Github::new(&h.config).unwrap())),
        });
        assert!(matches!(
            h.coordinator.state.merges["PR_1"],
            MergeProgress::Failed(_)
        ));
        h.assert_no_merge();
    }
}

#[tokio::test]
async fn label_toggles_are_queued_and_results_only_change_the_requested_label() {
    let mut h = Harness::new();
    h.prs
        .get_mut("PR_1")
        .unwrap()
        .snapshot
        .labels
        .push(crate::model::PrLabel {
            name: "second".into(),
            color: "00ff00".into(),
        });
    h.coordinator.state.labels.insert(
        "PR_1".into(),
        Labels {
            items: vec![
                Label {
                    name: "first".into(),
                    color: "ff0000".into(),
                    selected: false,
                },
                Label {
                    name: "second".into(),
                    color: "00ff00".into(),
                    selected: true,
                },
            ],
            ..Default::default()
        },
    );
    h.request(Request::Label {
        pr: "PR_1".into(),
        name: "first".into(),
        selected: true,
    });
    h.request(Request::Label {
        pr: "PR_1".into(),
        name: "second".into(),
        selected: false,
    });
    assert_eq!(h.coordinator.state.labels["PR_1"].unsaved.len(), 2);
    assert!(!h.directory.path().join("labels.log").exists());
    h.request(Request::SaveLabels {
        pr: "PR_1".into(),
        name: None,
    });
    assert_eq!(h.coordinator.state.labels["PR_1"].pending.len(), 2);
    while !h.coordinator.state.labels["PR_1"].pending.is_empty() {
        h.step().await;
    }
    let labels = &h.coordinator.state.labels["PR_1"];
    assert!(labels.pending.is_empty());
    assert!(labels.items[0].selected);
    assert!(!labels.items[1].selected);
    let calls = std::fs::read_to_string(h.directory.path().join("labels.log")).unwrap();
    let calls = calls.lines().collect::<Vec<_>>();
    assert!(calls[0].contains("--method POST"));
    assert!(calls[1].contains("--method DELETE"));
}

#[tokio::test]
async fn toggling_a_label_back_clears_the_draft_without_a_mutation() {
    let mut h = Harness::new();
    h.coordinator.state.labels.insert(
        "PR_1".into(),
        Labels {
            items: vec![Label {
                name: "one".into(),
                color: "ff0000".into(),
                selected: false,
            }],
            ..Default::default()
        },
    );
    for selected in [true, false] {
        h.request(Request::Label {
            pr: "PR_1".into(),
            name: "one".into(),
            selected,
        });
    }
    assert!(h.coordinator.state.labels["PR_1"].unsaved.is_empty());
    h.request(Request::SaveLabels {
        pr: "PR_1".into(),
        name: None,
    });
    assert!(h.coordinator.state.labels["PR_1"].pending.is_empty());
    assert!(!h.directory.path().join("labels.log").exists());
}

#[tokio::test]
async fn saving_one_draft_leaves_other_drafts_unsaved() {
    let mut h = Harness::new();
    h.coordinator.state.labels.insert(
        "PR_1".into(),
        Labels {
            items: ["one", "two"]
                .into_iter()
                .map(|name| Label {
                    name: name.into(),
                    color: "ff0000".into(),
                    selected: false,
                })
                .collect(),
            ..Default::default()
        },
    );
    for name in ["one", "two"] {
        h.request(Request::Label {
            pr: "PR_1".into(),
            name: name.into(),
            selected: true,
        });
    }
    h.request(Request::SaveLabels {
        pr: "PR_1".into(),
        name: Some("one".into()),
    });
    assert!(h.coordinator.state.labels["PR_1"].pending.contains("one"));
    assert!(h.coordinator.state.labels["PR_1"].unsaved.contains("two"));
    while !h.coordinator.state.labels["PR_1"].pending.is_empty() {
        h.step().await;
    }
    assert!(h.coordinator.state.labels["PR_1"].unsaved.contains("two"));
    let calls = std::fs::read_to_string(h.directory.path().join("labels.log")).unwrap();
    assert_eq!(calls.lines().count(), 1);
}

#[tokio::test]
async fn draft_survives_refresh_and_clears_when_remote_selection_matches() {
    let mut h = Harness::new();
    h.coordinator
        .catalogues
        .insert("owner/repo".into(), catalogue(&["one"]));
    h.sync_labels();
    h.request(Request::Label {
        pr: "PR_1".into(),
        name: "one".into(),
        selected: true,
    });
    h.sync_labels();
    let labels = &h.coordinator.state.labels["PR_1"];
    assert!(labels.items[0].selected);
    assert!(labels.unsaved.contains("one"));
    h.prs.get_mut("PR_1").unwrap().snapshot.labels = catalogue(&["one"]).labels;
    h.sync_labels();
    assert!(h.coordinator.state.labels["PR_1"].unsaved.is_empty());
    h.request(Request::SaveLabels {
        pr: "PR_1".into(),
        name: None,
    });
    assert!(!h.directory.path().join("labels.log").exists());
}

#[tokio::test]
async fn disabling_labels_keeps_failed_changes_for_retry_without_sending_mutations() {
    let mut h = Harness::new();
    h.coordinator.state.labels.insert(
        "PR_1".into(),
        Labels {
            items: vec![Label {
                name: "one".into(),
                color: "ff0000".into(),
                selected: false,
            }],
            ..Default::default()
        },
    );
    h.request(Request::Label {
        pr: "PR_1".into(),
        name: "one".into(),
        selected: true,
    });
    h.request(Request::SaveLabels {
        pr: "PR_1".into(),
        name: None,
    });
    h.request(Request::Configure {
        repo: "owner/repo".into(),
        change: Setting::Enabled(Kind::Label, false),
    });
    h.step().await;
    let labels = &h.coordinator.state.labels["PR_1"];
    assert!(labels.items[0].selected);
    assert!(labels.pending.is_empty());
    assert!(labels.unsaved.contains("one"));
    assert!(labels.error.is_some());
    assert!(!h.directory.path().join("labels.log").exists());
}

#[tokio::test]
async fn changes_during_final_authentication_cannot_submit_a_merge() {
    for change in [
        "disable",
        "method",
        "reviewing",
        "comments",
        "cancel",
        "account",
    ] {
        let mut h = Harness::new();
        let gh = h.config.gh_path.as_ref().unwrap();
        let script = std::fs::read_to_string(gh)
            .unwrap()
            .replace(
                "case \"$input\" in *viewer*) echo",
                r#"case "$input" in *viewer*)
if [ -f "$(dirname "$0")/first-auth" ]; then
    touch "$(dirname "$0")/final-auth-started"
    while [ ! -f "$(dirname "$0")/release-auth" ]; do sleep 0.01; done
fi
touch "$(dirname "$0")/first-auth"
echo"#,
            )
            .replace("\"isResolved\":false", "\"isResolved\":true");
        std::fs::write(gh, script).unwrap();
        h.refresh_pr().await;
        h.request(Request::Configure {
            repo: "owner/repo".into(),
            change: Setting::Condition(Kind::Merge, Condition::Approved),
        });
        let token = h.start_merge();
        h.handle(ActionCommand::Tick {
            pr: "PR_1".into(),
            token,
            remaining: 0,
        });
        tokio::time::timeout(Duration::from_secs(8), async {
            while !h.directory.path().join("final-auth-started").exists() {
                tokio::select! {
                    command = h.receiver.recv() => {
                        if let Some(Command::PrAction(command)) = command { h.handle(command); }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
            }
        })
        .await
        .unwrap();
        match change {
            "disable" => h.request(Request::Configure {
                repo: "owner/repo".into(),
                change: Setting::Enabled(Kind::Merge, false),
            }),
            "method" => h.request(Request::Configure {
                repo: "owner/repo".into(),
                change: Setting::MergeMethod(MergeMethod::Squash),
            }),
            "cancel" => h.request(Request::CancelMerge("PR_1".into())),
            "account" => h.viewer = "another-account",
            state => {
                h.prs.get_mut("PR_1").unwrap().state = if state == "reviewing" {
                    crate::model::State::Reviewing
                } else {
                    crate::model::State::Comments
                }
            }
        }
        std::fs::write(h.directory.path().join("release-auth"), "").unwrap();
        // Process the authentication result after the worker-owned state changed.
        h.step().await;
        assert!(
            !h.directory.path().join("merge.json").exists(),
            "merge submitted after {change}"
        );
        assert!(h.coordinator.merges.is_empty());
    }
}

#[tokio::test]
async fn always_merge_rejects_changed_evidence_in_cache_or_final_snapshot() {
    for cached in [true, false] {
        let mut h = Harness::new();
        h.refresh_pr().await;
        let token = h.start_merge();
        let original = h.prs["PR_1"].clone();
        let mut changed = original.snapshot.clone();
        changed.threads[0].last_comment_id = "new-comment-on-same-head".into();
        let snapshot = if cached {
            h.prs.insert(
                "PR_1".into(),
                transition(changed, Some(&original), None, 130, 0),
            );
            original.snapshot.clone()
        } else {
            changed
        };
        h.handle(ActionCommand::MergeChecked {
            pr: "PR_1".into(),
            token,
            result: Ok((Box::new(snapshot), Github::new(&h.config).unwrap())),
        });
        assert!(matches!(
            h.coordinator.state.merges["PR_1"],
            MergeProgress::Failed(_)
        ));
        assert!(h.coordinator.merges.is_empty());
        h.assert_no_merge();
    }
}

#[tokio::test]
async fn label_authentication_finishes_before_the_final_worker_validation() {
    for change in ["disable", "stale", "condition", "account", "unchanged"] {
        let mut h = Harness::new();
        let gh = h.config.gh_path.as_ref().unwrap();
        let script = std::fs::read_to_string(gh).unwrap().replace(
            "case \"$input\" in *viewer*) echo",
            r#"case "$input" in *viewer*)
echo auth >> "$(dirname "$0")/auth-calls"
touch "$(dirname "$0")/auth-started"
while [ ! -f "$(dirname "$0")/release-auth" ]; do sleep 0.01; done
echo"#,
        );
        std::fs::write(gh, script).unwrap();
        h.coordinator.state.labels.insert(
            "PR_1".into(),
            Labels {
                items: vec![Label {
                    name: "one".into(),
                    color: "ff0000".into(),
                    selected: false,
                }],
                ..Default::default()
            },
        );
        h.request(Request::Label {
            pr: "PR_1".into(),
            name: "one".into(),
            selected: true,
        });
        h.request(Request::SaveLabels {
            pr: "PR_1".into(),
            name: None,
        });
        tokio::time::timeout(Duration::from_secs(8), async {
            while !h.directory.path().join("auth-started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        match change {
            "disable" => h.request(Request::Configure {
                repo: "owner/repo".into(),
                change: Setting::Enabled(Kind::Label, false),
            }),
            "stale" => h.prs.get_mut("PR_1").unwrap().stale = true,
            "condition" => h.request(Request::Configure {
                repo: "owner/repo".into(),
                change: Setting::Condition(Kind::Label, Condition::Approved),
            }),
            "account" => h.viewer = "another-account",
            _ => {}
        }
        std::fs::write(h.directory.path().join("release-auth"), "").unwrap();
        h.step().await;
        if change == "unchanged" {
            h.step().await;
        }
        assert_eq!(
            h.directory.path().join("labels.log").exists(),
            change == "unchanged"
        );
        assert_eq!(
            std::fs::read_to_string(h.directory.path().join("auth-calls"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn merge_rejects_unseen_same_head_evidence_before_starting_countdown() {
    let mut h = Harness::new();
    h.refresh_pr().await;
    let displayed = h.prs["PR_1"].clone();
    let mut changed = displayed.snapshot.clone();
    changed.threads[0].last_comment_id = "posted-while-menu-open".into();
    let current = transition(changed, Some(&displayed), None, 130, 0);
    assert_ne!(current.update_id, displayed.update_id);
    assert_eq!(current.snapshot.head, displayed.snapshot.head);
    h.prs.insert("PR_1".into(), current);
    h.request(Request::Merge {
        on_green: false,
        pr: "PR_1".into(),
        head: displayed.snapshot.head,
        update: displayed.update_id,
    });
    assert!(h.coordinator.merges.is_empty());
    assert!(h.coordinator.state.merges.is_empty());
    assert!(
        h.coordinator
            .state
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("review changed")
    );
    h.assert_no_merge();
    // Reopening the menu with current evidence can start a new countdown.
    h.start_merge();
    assert_eq!(
        h.coordinator.state.merges["PR_1"],
        MergeProgress::Countdown(5)
    );
    h.request(Request::CancelMerge("PR_1".into()));
}

#[tokio::test]
async fn label_receipt_follows_pending_or_rejected_action_state() {
    for accepted in [true, false] {
        let mut h = Harness::new();
        if accepted {
            h.coordinator.state.labels.insert(
                "PR_1".into(),
                Labels {
                    items: vec![Label {
                        name: "one".into(),
                        color: "ff0000".into(),
                        selected: false,
                    }],
                    ..Default::default()
                },
            );
        }
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        h.sink = Arc::new(move |event| captured.lock().unwrap().push(event));
        h.request(Request::Label {
            pr: "PR_1".into(),
            name: "one".into(),
            selected: true,
        });
        let events = events.lock().unwrap();
        assert!(matches!(events.last(), Some(UiEvent::LabelRequestHandled)));
        let UiEvent::ActionsChanged(state) = &events[events.len() - 2] else {
            panic!("Receipt must follow action state");
        };
        if accepted {
            assert!(state.labels["PR_1"].unsaved.contains("one"));
        } else {
            assert!(state.error.is_some());
        }
    }
}

impl Harness {
    fn sync_labels(&mut self) {
        self.coordinator.reconcile(&Context {
            store: &self.store,
            prs: &self.prs,
            viewer: Some(self.viewer),
            config: &self.config,
            sender: &self.sender,
            sink: &self.sink,
        });
    }
    fn refresh_catalogues(&mut self) {
        self.coordinator.refresh_catalogues(
            &Context {
                store: &self.store,
                prs: &self.prs,
                viewer: Some(self.viewer),
                config: &self.config,
                sender: &self.sender,
                sink: &self.sink,
            },
            None,
        );
    }
}

fn catalogue(names: &[&str]) -> LabelCatalogue {
    LabelCatalogue {
        labels: names
            .iter()
            .map(|name| crate::model::PrLabel {
                name: (*name).into(),
                color: "112233".into(),
            })
            .collect(),
        fetched_at: chrono::Utc::now().timestamp(),
    }
}

#[tokio::test]
async fn catalogue_is_shared_persisted_and_refreshed_only_when_due_or_requested() {
    let mut h = Harness::new();
    std::fs::write(h.config.gh_path.as_ref().unwrap(), r#"#!/bin/sh
if [ "$1" = auth ]; then echo test-credential; exit 0; fi
case "$*" in
  *graphql*) cat >/dev/null; echo '{"data":{"viewer":{"login":"test"}}}';;
  *repos/owner/repo/labels*) echo fetch >> "$(dirname "$0")/catalogue.log"; echo '[{"name":"bug","color":"ff0000"}]';;
  *) exit 1;;
esac
"#).unwrap();
    let mut second = h.prs["PR_1"].clone();
    second.snapshot.id = "PR_2".into();
    h.prs.insert("PR_2".into(), second);
    h.prs.get_mut("PR_1").unwrap().snapshot.labels = catalogue(&["bug"]).labels;
    h.sync_labels();
    assert!(h.coordinator.state.labels["PR_1"].items[0].selected);
    assert!(!h.coordinator.state.labels["PR_1"].catalogue_ready);
    h.refresh_catalogues();
    let token = h.coordinator.loads["owner/repo"];
    h.request(Request::LoadLabels("PR_2".into()));
    assert_eq!(h.coordinator.loads["owner/repo"], token);
    h.step().await;
    assert!(h.coordinator.loads.is_empty());
    assert!(h.coordinator.state.labels["PR_1"].catalogue_ready);
    assert!(h.coordinator.state.labels["PR_1"].items[0].selected);
    assert!(!h.coordinator.state.labels["PR_2"].items[0].selected);
    h.refresh_catalogues();
    assert!(h.coordinator.loads.is_empty());
    assert_eq!(
        std::fs::read_to_string(h.directory.path().join("catalogue.log"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    h.coordinator = Coordinator::new(&h.store).unwrap();
    assert!(h.coordinator.state.labels.is_empty());
    h.refresh_catalogues();
    let labels = &h.coordinator.state.labels["PR_1"];
    assert!(labels.catalogue_ready);
    assert!(!labels.loading);
    assert!(labels.items[0].selected);
    assert!(!h.coordinator.state.labels["PR_2"].items[0].selected);
    assert!(
        h.coordinator.loads.is_empty(),
        "Restart must reuse a fresh persisted catalogue"
    );
    h.coordinator
        .catalogues
        .get_mut("owner/repo")
        .unwrap()
        .fetched_at -= 901;
    h.refresh_catalogues();
    h.step().await;
    h.request(Request::LoadLabels("PR_1".into()));
    h.step().await;
    assert_eq!(
        std::fs::read_to_string(h.directory.path().join("catalogue.log"))
            .unwrap()
            .lines()
            .count(),
        3
    );
}

#[test]
fn catalogue_refresh_preserves_pending_toggles_and_uses_latest_snapshot_assignments() {
    let mut h = Harness::new();
    h.coordinator
        .catalogues
        .insert("owner/repo".into(), catalogue(&["bug", "ready"]));
    h.prs.get_mut("PR_1").unwrap().snapshot.labels = catalogue(&["bug"]).labels;
    h.sync_labels();
    let labels = h.coordinator.state.labels.get_mut("PR_1").unwrap();
    labels.items[0].selected = false;
    labels.pending.insert("bug".into());
    labels.items[1].selected = true;
    h.coordinator
        .completed_labels
        .insert(("PR_1".into(), "ready".into()), true);
    h.coordinator.loads.insert("owner/repo".into(), 42);
    h.handle(ActionCommand::LabelsLoaded {
        repo: "owner/repo".into(),
        token: 42,
        result: Ok(catalogue(&["ready", "new"]).labels),
    });
    let labels = &h.coordinator.state.labels["PR_1"];
    assert!(
        !labels
            .items
            .iter()
            .find(|l| l.name == "bug")
            .unwrap()
            .selected
    );
    assert!(
        labels
            .items
            .iter()
            .find(|l| l.name == "ready")
            .unwrap()
            .selected
    );
    assert!(labels.pending.contains("bug"));
    // Once LabelSaved updates the snapshot, subsequent polls control the selection again.
    h.prs.get_mut("PR_1").unwrap().snapshot.labels = catalogue(&["new", "ready"]).labels;
    h.coordinator.label_saved("PR_1", "ready");
    h.sync_labels();
    assert!(
        h.coordinator.state.labels["PR_1"]
            .items
            .iter()
            .find(|l| l.name == "new")
            .unwrap()
            .selected
    );
    h.prs.get_mut("PR_1").unwrap().snapshot.labels.clear();
    h.sync_labels();
    assert!(
        !h.coordinator.state.labels["PR_1"]
            .items
            .iter()
            .find(|l| l.name == "ready")
            .unwrap()
            .selected
    );
}

#[test]
fn catalogue_errors_keep_cached_labels_and_do_not_hide_mutation_errors() {
    let mut h = Harness::new();
    h.coordinator
        .catalogues
        .insert("owner/repo".into(), catalogue(&["bug"]));
    h.sync_labels();
    let labels = h.coordinator.state.labels.get_mut("PR_1").unwrap();
    labels.items[0].selected = true;
    labels.unsaved.insert("bug".into());
    labels.error = Some("Mutation failed".into());
    h.coordinator.loads.insert("owner/repo".into(), 1);
    h.handle(ActionCommand::LabelsLoaded {
        repo: "owner/repo".into(),
        token: 1,
        result: Err("Offline".into()),
    });
    let labels = &h.coordinator.state.labels["PR_1"];
    assert_eq!(labels.items.len(), 1);
    assert_eq!(labels.catalogue_error.as_deref(), Some("Offline"));
    h.coordinator.loads.insert("owner/repo".into(), 2);
    h.handle(ActionCommand::LabelsLoaded {
        repo: "owner/repo".into(),
        token: 2,
        result: Ok(catalogue(&["bug", "ready"]).labels),
    });
    let labels = &h.coordinator.state.labels["PR_1"];
    assert_eq!(labels.items.len(), 2);
    assert!(labels.catalogue_error.is_none());
    assert_eq!(labels.error.as_deref(), Some("Mutation failed"));
}

#[test]
fn resolved_label_error_clears_when_the_last_pending_job_finishes() {
    let mut h = Harness::new();
    h.coordinator
        .catalogues
        .insert("owner/repo".into(), catalogue(&["bug"]));
    h.sync_labels();
    let labels = h.coordinator.state.labels.get_mut("PR_1").unwrap();
    labels.pending.insert("other".into());
    labels.error = Some("Mutation failed".into());
    h.sync_labels();
    assert_eq!(
        h.coordinator.state.labels["PR_1"].error.as_deref(),
        Some("Mutation failed")
    );
    h.coordinator
        .state
        .labels
        .get_mut("PR_1")
        .unwrap()
        .pending
        .clear();
    h.sync_labels();
    assert!(h.coordinator.state.labels["PR_1"].error.is_none());
}

#[test]
fn catalogue_cache_and_in_flight_results_are_scoped_to_account() {
    let mut h = Harness::new();
    h.store
        .save_label_catalogue("test", "OWNER/REPO", &catalogue(&["private"]))
        .unwrap();
    h.coordinator = Coordinator::new(&h.store).unwrap();
    h.sync_labels();
    assert_eq!(h.coordinator.state.labels["PR_1"].items.len(), 1);
    h.coordinator.loads.insert("owner/repo".into(), 1);
    h.viewer = "other";
    h.sync_labels();
    h.handle(ActionCommand::LabelsLoaded {
        repo: "owner/repo".into(),
        token: 1,
        result: Ok(catalogue(&["late"]).labels),
    });
    assert!(h.coordinator.state.labels["PR_1"].items.is_empty());
    assert!(h.store.label_catalogues("other").unwrap().is_empty());
    assert_eq!(
        h.store.label_catalogues("test").unwrap()["owner/repo"].labels[0].name,
        "private"
    );
}

impl Harness {
    fn queue_merge(&mut self) -> u64 {
        self.prs.get_mut("PR_1").unwrap().snapshot.check_state = Some(CheckState::Running);
        self.request(Request::Merge {
            pr: "PR_1".into(),
            head: self.prs["PR_1"].snapshot.head.clone(),
            update: self.prs["PR_1"].update_id.clone(),
            on_green: true,
        });
        self.coordinator.merges["PR_1"].token
    }
}

#[tokio::test]
async fn queued_merge_waits_then_counts_down_and_merges_once() {
    let mut h = Harness::new();
    h.refresh_pr().await;
    let token = h.queue_merge();
    assert_eq!(
        h.coordinator.state.merges["PR_1"],
        MergeProgress::WaitingForChecks
    );
    h.handle(ActionCommand::Tick {
        pr: "PR_1".into(),
        token,
        remaining: 0,
    });
    assert_eq!(
        h.coordinator.state.merges["PR_1"],
        MergeProgress::WaitingForChecks
    );
    h.assert_no_merge();
    h.prs.get_mut("PR_1").unwrap().snapshot.check_state = Some(CheckState::Green);
    h.handle(ActionCommand::Tick {
        pr: "PR_1".into(),
        token,
        remaining: 4,
    });
    assert_eq!(
        h.coordinator.state.merges["PR_1"],
        MergeProgress::Countdown(5)
    );
    let snapshot = h.prs["PR_1"].snapshot.clone();
    h.handle(ActionCommand::MergeChecked {
        pr: "PR_1".into(),
        token,
        result: Ok((Box::new(snapshot), Github::new(&h.config).unwrap())),
    });
    while h.coordinator.has_submissions() {
        h.step().await;
    }
    assert_eq!(h.coordinator.state.merges["PR_1"], MergeProgress::Complete);
    assert!(h.directory.path().join("merge.json").exists());
}

#[tokio::test]
async fn queued_merge_cancels_on_invalidated_evidence_and_shutdown() {
    for case in 0..10 {
        let mut h = Harness::new();
        let token = h.queue_merge();
        let pr = h.prs.get_mut("PR_1").unwrap();
        match case {
            0 => pr.snapshot.check_state = Some(CheckState::Failed),
            1 => pr.snapshot.check_state = Some(CheckState::Conflicts),
            2 => pr.snapshot.check_state = None,
            3 => pr.stale = true,
            4 => pr.update_id = "changed".into(),
            5 => pr.snapshot.head = "changed".into(),
            6 => pr.snapshot.open = false,
            7 => h.viewer = "changed",
            8 => h.request(Request::CancelMerge("PR_1".into())),
            _ => h.coordinator.begin_shutdown(),
        }
        h.handle(ActionCommand::Tick {
            pr: "PR_1".into(),
            token,
            remaining: 0,
        });
        assert!(h.coordinator.merges.is_empty(), "case {case}");
        h.assert_no_merge();
    }
}

#[tokio::test]
async fn queued_merge_requires_green_in_final_snapshot() {
    for check_state in [
        None,
        Some(CheckState::Running),
        Some(CheckState::Failed),
        Some(CheckState::Conflicts),
    ] {
        let mut h = Harness::new();
        h.refresh_pr().await;
        let token = h.queue_merge();
        h.prs.get_mut("PR_1").unwrap().snapshot.check_state = Some(CheckState::Green);
        h.handle(ActionCommand::Tick {
            pr: "PR_1".into(),
            token,
            remaining: 4,
        });
        let mut snapshot = h.prs["PR_1"].snapshot.clone();
        snapshot.check_state = check_state;
        h.handle(ActionCommand::MergeChecked {
            pr: "PR_1".into(),
            token,
            result: Ok((Box::new(snapshot), Github::new(&h.config).unwrap())),
        });
        assert!(matches!(
            h.coordinator.state.merges["PR_1"],
            MergeProgress::Failed(_)
        ));
        h.assert_no_merge();
    }
}

#[tokio::test]
async fn captured_queue_mode_is_preserved_when_checks_change_before_dispatch() {
    for check_state in [CheckState::Green, CheckState::Failed] {
        let mut h = Harness::new();
        h.prs.get_mut("PR_1").unwrap().snapshot.check_state = Some(check_state);
        h.request(Request::Merge {
            pr: "PR_1".into(),
            head: "head".into(),
            update: h.prs["PR_1"].update_id.clone(),
            on_green: true,
        });
        if check_state == CheckState::Green {
            assert_eq!(
                h.coordinator.state.merges["PR_1"],
                MergeProgress::Countdown(5)
            );
        } else {
            assert!(h.coordinator.merges.is_empty());
        }
        h.assert_no_merge();
    }
}

#[tokio::test]
async fn cancelled_queue_ignores_late_validation_after_requeue() {
    let mut h = Harness::new();
    h.refresh_pr().await;
    let old = h.queue_merge();
    h.request(Request::CancelMerge("PR_1".into()));
    h.queue_merge();
    let mut snapshot = h.prs["PR_1"].snapshot.clone();
    snapshot.check_state = Some(CheckState::Green);
    h.handle(ActionCommand::MergeChecked {
        pr: "PR_1".into(),
        token: old,
        result: Ok((Box::new(snapshot), Github::new(&h.config).unwrap())),
    });
    assert_eq!(
        h.coordinator.state.merges["PR_1"],
        MergeProgress::WaitingForChecks
    );
    h.assert_no_merge();
}

async fn two_queued_merges() -> Harness {
    let mut h = Harness::new();
    h.refresh_pr().await;
    h.add_pr("PR_2", 2).await;
    assert_eq!(h.prs["PR_2"].snapshot.base_branch.as_deref(), Some("main"));
    h.merge("PR_1");
    h.merge("PR_2");
    h.finish_countdown("PR_1");
    h.finish_countdown("PR_2");
    assert_eq!(h.progress("PR_1"), Some(&MergeProgress::Checking));
    assert_eq!(h.progress("PR_2"), Some(&MergeProgress::Queued(1)));
    h
}

#[tokio::test]
async fn merges_into_the_same_base_branch_are_submitted_one_at_a_time() {
    let mut h = two_queued_merges().await;
    for _ in 0..40 {
        if h.progress("PR_2") == Some(&MergeProgress::Complete) {
            break;
        }
        if matches!(
            h.progress("PR_2"),
            Some(MergeProgress::Checking | MergeProgress::Merging)
        ) {
            assert_eq!(h.progress("PR_1"), Some(&MergeProgress::Complete));
        }
        h.step().await;
    }
    assert_eq!(h.progress("PR_1"), Some(&MergeProgress::Complete));
    assert_eq!(h.progress("PR_2"), Some(&MergeProgress::Complete));
    let log = h.merge_log();
    assert_eq!(log.len(), 2);
    assert!(log[0].contains("pulls/1/merge"));
    assert!(log[1].contains("pulls/2/merge"));
}

#[tokio::test]
async fn a_failed_merge_does_not_block_the_next_queued_merge() {
    let mut h = Harness::new();
    h.patch_gh(
        " *'--method PUT'*)",
        " *pulls/1/merge*) cat >/dev/null; echo '{\"merged\":false,\"message\":\"Blocked by branch protection\"}'; exit 0;;\n *'--method PUT'*)",
    );
    h.refresh_pr().await;
    h.add_pr("PR_2", 2).await;
    h.merge("PR_1");
    h.merge("PR_2");
    h.finish_countdown("PR_1");
    h.finish_countdown("PR_2");
    assert_eq!(h.progress("PR_2"), Some(&MergeProgress::Queued(1)));
    for _ in 0..40 {
        if h.progress("PR_2") == Some(&MergeProgress::Complete) {
            break;
        }
        h.step().await;
    }
    assert!(matches!(
        h.progress("PR_1"),
        Some(MergeProgress::Failed(failure))
            if failure.message == "GitHub: Blocked by branch protection"
    ));
    assert_eq!(h.progress("PR_2"), Some(&MergeProgress::Complete));
    let log = h.merge_log();
    assert_eq!(log.len(), 1, "a failed merge must not be retried");
    assert!(log[0].contains("pulls/2/merge"));
}

#[tokio::test]
async fn merges_into_different_base_branches_run_independently() {
    let mut h = Harness::new();
    h.refresh_pr().await;
    h.add_pr("PR_2", 2).await;
    h.add_pr("PR_3", 3).await;
    h.prs.get_mut("PR_2").unwrap().snapshot.base_branch = Some("release".into());
    h.prs.get_mut("PR_3").unwrap().snapshot.repo = "owner/other".into();
    let preferences = h.coordinator.state.preferences("owner/repo");
    h.coordinator
        .state
        .preferences
        .insert(repo_key("owner/other"), preferences);
    for pr in ["PR_1", "PR_2", "PR_3"] {
        h.merge(pr);
    }
    for pr in ["PR_1", "PR_2", "PR_3"] {
        h.finish_countdown(pr);
        assert_eq!(h.progress(pr), Some(&MergeProgress::Checking));
    }
}

#[tokio::test]
async fn an_unknown_base_branch_shares_a_queue_with_the_whole_repository() {
    let mut h = Harness::new();
    h.refresh_pr().await;
    h.add_pr("PR_2", 2).await;
    h.prs.get_mut("PR_2").unwrap().snapshot.base_branch = None;
    h.merge("PR_1");
    h.merge("PR_2");
    h.finish_countdown("PR_1");
    h.finish_countdown("PR_2");
    assert_eq!(h.progress("PR_1"), Some(&MergeProgress::Checking));
    assert_eq!(h.progress("PR_2"), Some(&MergeProgress::Queued(1)));
}

#[tokio::test]
async fn countdown_delivery_order_cannot_overtake_an_earlier_request() {
    let mut h = Harness::new();
    h.refresh_pr().await;
    h.add_pr("PR_2", 2).await;
    h.merge("PR_1");
    h.merge("PR_2");
    // Countdowns started together may deliver their final ticks in any order.
    h.finish_countdown("PR_2");
    assert_eq!(h.progress("PR_2"), Some(&MergeProgress::Queued(1)));
    h.finish_countdown("PR_1");
    assert_eq!(h.progress("PR_1"), Some(&MergeProgress::Checking));
    assert_eq!(h.progress("PR_2"), Some(&MergeProgress::Queued(1)));
}

#[tokio::test]
async fn a_merge_waiting_for_checks_does_not_hold_the_queue() {
    let mut h = Harness::new();
    h.refresh_pr().await;
    h.add_pr("PR_2", 2).await;
    h.prs.get_mut("PR_1").unwrap().snapshot.check_state = Some(crate::model::CheckState::Running);
    h.request(Request::Merge {
        on_green: true,
        pr: "PR_1".into(),
        head: "head".into(),
        update: h.prs["PR_1"].update_id.clone(),
    });
    assert_eq!(h.progress("PR_1"), Some(&MergeProgress::WaitingForChecks));
    // Checks may never turn green, so a ready merge must not wait behind them.
    h.merge("PR_2");
    h.finish_countdown("PR_2");
    assert_eq!(h.progress("PR_2"), Some(&MergeProgress::Checking));
}

#[tokio::test]
async fn retargeting_a_pending_merge_cancels_it() {
    let mut h = two_queued_merges().await;
    h.prs.get_mut("PR_2").unwrap().snapshot.base_branch = Some("release".into());
    h.reconcile();
    assert!(matches!(h.progress("PR_2"), Some(MergeProgress::Failed(_))));
    assert!(!h.coordinator.merges.contains_key("PR_2"));

    let mut h = Harness::new();
    h.refresh_pr().await;
    let token = h.start_merge();
    let mut snapshot = h.prs["PR_1"].snapshot.clone();
    snapshot.base_branch = Some("release".into());
    h.handle(ActionCommand::MergeChecked {
        pr: "PR_1".into(),
        token,
        result: Ok((Box::new(snapshot), Github::new(&h.config).unwrap())),
    });
    assert!(matches!(h.progress("PR_1"), Some(MergeProgress::Failed(_))));
    h.assert_no_merge();
}

#[tokio::test]
async fn queued_merges_start_in_request_order() {
    let mut h = Harness::new();
    h.refresh_pr().await;
    h.add_pr("PR_2", 2).await;
    h.add_pr("PR_3", 3).await;
    h.merge("PR_1");
    h.merge("PR_3");
    h.merge("PR_2");
    h.finish_countdown("PR_1");
    h.finish_countdown("PR_2");
    h.finish_countdown("PR_3");
    assert_eq!(h.progress("PR_2"), Some(&MergeProgress::Queued(1)));
    assert_eq!(h.progress("PR_3"), Some(&MergeProgress::Queued(1)));
    h.request(Request::CancelMerge("PR_1".into()));
    assert_eq!(h.progress("PR_3"), Some(&MergeProgress::Checking));
    assert_eq!(h.progress("PR_2"), Some(&MergeProgress::Queued(3)));
}

#[tokio::test]
async fn queued_merges_can_be_cancelled_or_invalidated_without_submission() {
    for case in 0..3 {
        let mut h = two_queued_merges().await;
        match case {
            0 => h.request(Request::CancelMerge("PR_2".into())),
            1 => {
                h.prs.get_mut("PR_2").unwrap().snapshot.check_state =
                    Some(crate::model::CheckState::Conflicts);
                h.reconcile();
                assert!(matches!(h.progress("PR_2"), Some(MergeProgress::Failed(_))));
            }
            _ => {
                h.prs.get_mut("PR_2").unwrap().snapshot.head = "new-head".into();
                h.reconcile();
            }
        }
        assert!(!h.coordinator.merges.contains_key("PR_2"));
        for _ in 0..40 {
            if h.progress("PR_1") == Some(&MergeProgress::Complete) {
                break;
            }
            h.step().await;
        }
        assert_eq!(h.progress("PR_1"), Some(&MergeProgress::Complete));
        let log = h.merge_log();
        assert_eq!(log.len(), 1, "case {case}");
        assert!(log[0].contains("pulls/1/merge"));
    }
}

#[tokio::test]
async fn shutdown_drains_the_submitted_merge_but_drops_the_queue() {
    let mut h = two_queued_merges().await;
    while !h.coordinator.has_submissions() {
        h.step().await;
    }
    h.coordinator.begin_shutdown();
    while h.coordinator.has_submissions() {
        h.step().await;
    }
    assert!(!h.coordinator.merges.contains_key("PR_2"));
    let log = h.merge_log();
    assert_eq!(log.len(), 1);
    assert!(log[0].contains("pulls/1/merge"));
}

#[tokio::test]
async fn merge_waits_for_github_to_recalculate_mergeability() {
    let mergeability = |delay: &str, limit: u32| {
        format!(
            "case \"$input\" in *'query Mergeability'*) echo x >> \"$(dirname \"$0\")/mergeability.log\"; {delay} if [ $(wc -l < \"$(dirname \"$0\")/mergeability.log\") -le {limit} ]; then m=UNKNOWN; else m=MERGEABLE; fi; echo \"{{\\\"data\\\":{{\\\"node\\\":{{\\\"mergeable\\\":\\\"$m\\\"}}}}}}\"; exit 0;; esac\ncase \"$input\" in *viewer*)"
        )
    };
    // Recalculated after two reads; never recalculated; one slow request.
    for (delay, limit) in [("", 2), ("", 1000), ("sleep 5;", 1000)] {
        let mut h = Harness::new();
        h.patch_gh("case \"$input\" in *viewer*)", &mergeability(delay, limit));
        h.refresh_pr().await;
        h.start_merge();
        let start = std::time::Instant::now();
        h.finish_countdown("PR_1");
        for _ in 0..20 {
            if h.progress("PR_1") == Some(&MergeProgress::Complete) {
                break;
            }
            h.step().await;
        }
        assert_eq!(h.progress("PR_1"), Some(&MergeProgress::Complete));
        // The wall-clock bound includes request latency.
        assert!(start.elapsed() < Duration::from_secs(4), "{delay:?}");
        let polls = std::fs::read_to_string(h.directory.path().join("mergeability.log"))
            .unwrap()
            .lines()
            .count();
        match (delay, limit) {
            ("", 2) => assert_eq!(polls, 3),
            ("", _) => assert!(polls > 2),
            _ => assert_eq!(polls, 1),
        }
        assert_eq!(h.merge_log().len(), 1);
    }
}
