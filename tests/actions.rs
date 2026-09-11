use gopher::{
    actions::*,
    model::{CheckState, PullRequest, Snapshot, State},
    store::Store,
};

#[test]
fn preferences_are_disabled_by_default_and_persist_per_repository_without_pr_cache() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(directory.path()).unwrap();
    let mut preferences = Preferences::default();
    assert!(!preferences.merge.enabled && !preferences.label.enabled);
    assert_eq!(preferences.merge_method, MergeMethod::Merge);
    preferences.apply(&Setting::Enabled(Kind::Merge, true));
    preferences.apply(&Setting::MergeMethod(MergeMethod::Squash));
    preferences.apply(&Setting::Condition(Kind::Label, Condition::Comments));
    store
        .save_action_preferences("Owner/Repo", &preferences)
        .unwrap();
    store.set_viewer("first").unwrap();
    store.set_viewer("second").unwrap();
    store.retain(&Default::default()).unwrap();
    drop(store);
    let stored = Store::open(directory.path())
        .unwrap()
        .action_preferences()
        .unwrap();
    assert_eq!(stored["owner/repo"], preferences);
    assert_eq!(stored.len(), 1);
}

#[test]
fn conditions_match_exact_states_and_disable_stale_closed_or_draft_merges() {
    let mut pr = PullRequest::unreviewed(Snapshot {
        open: true,
        ..Default::default()
    });
    pr.stale = false;
    let mut preferences = Preferences::default();
    preferences.merge.enabled = true;
    preferences.label.enabled = true;
    for condition in Condition::ALL {
        preferences.merge.condition = condition;
        for state in [
            State::Unknown,
            State::Reviewing,
            State::Comments,
            State::Approved,
        ] {
            pr.state = state;
            assert_eq!(
                preferences.allows(Kind::Merge, &pr),
                condition == Condition::Always || condition.label() == state.label()
            );
        }
    }
    preferences.merge.condition = Condition::Always;
    pr.stale = true;
    assert!(!preferences.allows(Kind::Merge, &pr));
    assert!(!preferences.allows(Kind::Label, &pr));
    pr.stale = false;
    pr.snapshot.open = false;
    assert!(!preferences.allows(Kind::Merge, &pr));
    pr.snapshot.open = true;
    pr.snapshot.draft = true;
    assert!(!preferences.allows(Kind::Merge, &pr));
    assert!(preferences.allows(Kind::Label, &pr));
}

#[test]
fn conflicts_disable_merge_for_every_condition_but_allow_labels() {
    let mut pr = PullRequest::unreviewed(Snapshot {
        open: true,
        ..Default::default()
    });
    pr.stale = false;
    let mut preferences = Preferences::default();
    preferences.merge.enabled = true;
    preferences.label.enabled = true;
    for (condition, state) in [
        (Condition::Always, State::Unknown),
        (Condition::Reviewing, State::Reviewing),
        (Condition::Comments, State::Comments),
        (Condition::Approved, State::Approved),
    ] {
        preferences.merge.condition = condition;
        pr.state = state;
        for checks in [
            Some(CheckState::Conflicts),
            Some(CheckState::Green),
            Some(CheckState::Running),
            Some(CheckState::Failed),
            None,
        ] {
            pr.snapshot.check_state = checks;
            assert_eq!(
                preferences.allows(Kind::Merge, &pr),
                checks != Some(CheckState::Conflicts)
            );
            assert!(preferences.allows(Kind::Label, &pr));
        }
    }
}
