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
    fn assert_no_merge(&self) {
        assert!(!self.directory.path().join("merge.json").exists());
    }
}

#[tokio::test]
async fn merge_countdown_completes_without_ui_events_and_pins_the_commit() {
    let mut h = Harness::new();
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
async fn changed_commit_settings_account_or_ignore_cancels_pending_merge() {
    for case in 0..4 {
        let mut h = Harness::new();
        let token = h.start_merge();
        match case {
            0 => h.prs.get_mut("PR_1").unwrap().snapshot.head = "new".into(),
            1 => h.request(Request::Configure {
                repo: "owner/repo".into(),
                change: Setting::MergeMethod(MergeMethod::Squash),
            }),
            2 => h.viewer = "different-user",
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
async fn fresh_validation_cannot_merge_a_changed_head_or_a_closed_pr() {
    for case in 0..3 {
        let mut h = Harness::new();
        let token = h.start_merge();
        let mut snapshot = h.prs["PR_1"].snapshot.clone();
        match case {
            0 => snapshot.head = "new-head".into(),
            1 => snapshot.open = false,
            _ => snapshot.draft = true,
        }
        h.handle(ActionCommand::MergeChecked {
            pr: "PR_1".into(),
            token,
            result: Ok(Box::new(snapshot)),
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
