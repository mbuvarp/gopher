use super::*;
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
case "$input" in *viewer*) echo '{"data":{"viewer":{"login":"test"}}}'; exit 0;; esac
"#,
        );
        std::fs::write(&gh, format!(r#"#!/bin/sh
if [ "$1" = auth ]; then echo test-credential; exit 0; fi
case "$*" in
 *'--method PUT'*) cat > "$(dirname "$0")/merge.json"; echo '{{"merged":true}}'; exit 0;;
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
    fn start_merge(&mut self) -> u64 {
        self.request(Request::Merge {
            pr: "PR_1".into(),
            head: "head".into(),
            update: self.prs["PR_1"].update_id.clone(),
        });
        self.coordinator.merges["PR_1"].token
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
    async fn refresh_pr(&mut self) {
        let snapshot = Github::new(&self.config)
            .unwrap()
            .snapshot(&PrRef {
                id: "PR_1".into(),
                repo: "owner/repo".into(),
                number: 1,
            })
            .await
            .unwrap();
        self.prs
            .insert("PR_1".into(), transition(snapshot, None, None, 100, 0));
    }
    fn assert_no_merge(&self) {
        assert!(!self.directory.path().join("merge.json").exists());
    }
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
async fn disabling_labels_reverts_queued_toggles_without_sending_mutations() {
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
    h.request(Request::Configure {
        repo: "owner/repo".into(),
        change: Setting::Enabled(Kind::Label, false),
    });
    h.step().await;
    let labels = &h.coordinator.state.labels["PR_1"];
    assert!(!labels.items[0].selected);
    assert!(labels.pending.is_empty());
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
            .as_deref()
            .unwrap()
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
            assert!(state.labels["PR_1"].pending.contains("one"));
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
    h.coordinator.state.labels.get_mut("PR_1").unwrap().error = Some("Mutation failed".into());
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
