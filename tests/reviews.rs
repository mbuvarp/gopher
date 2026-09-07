use gopher::{model::*, reviewers, store::Store, worker::transition};

const HEAD: &str = "abcdef0123456789012345678901234567890123";
fn snapshot() -> Snapshot {
    Snapshot {
        id: "PR_test".into(),
        repo: "owner/repo".into(),
        number: 1,
        url: "https://github.com/owner/repo/pull/1".into(),
        head: HEAD.into(),
        open: true,
        ..Default::default()
    }
}
fn review(agent: Agent, body: &str) -> Review {
    Review {
        id: "review-1".into(),
        author: match agent {
            Agent::Cubic => "cubic-dev-ai[bot]",
            Agent::Codex => "chatgpt-codex-connector[bot]",
            Agent::CodeRabbit => "coderabbitai[bot]",
        }
        .into(),
        body: body.into(),
        state: "COMMENTED".into(),
        commit: HEAD.into(),
        submitted_at: "2026-09-07T16:30:00Z".into(),
    }
}
fn eyes() -> Reaction {
    Reaction {
        id: "eyes-1".into(),
        author: "chatgpt-codex-connector[bot]".into(),
        content: "EYES".into(),
        created_at: "2026-09-07T16:28:00Z".into(),
    }
}
fn thumb() -> Reaction {
    Reaction {
        id: "thumb-1".into(),
        content: "THUMBS_UP".into(),
        created_at: "2026-09-07T16:31:00Z".into(),
        ..eyes()
    }
}
fn thread() -> Thread {
    Thread {
        id: "thread-1".into(),
        author: "cubic-dev-ai[bot]".into(),
        last_comment_id: "comment-1".into(),
        ..Default::default()
    }
}
fn check(status: &str) -> Check {
    Check {
        id: "100".into(),
        app: "cubic-dev-ai".into(),
        name: "cubic · AI code reviewer".into(),
        status: status.into(),
        conclusion: "success".into(),
        started_at: "2026-09-07T16:28:00Z".into(),
        ..Default::default()
    }
}
fn state(s: &Snapshot) -> State {
    reviewers::aggregate(s, &reviewers::evaluate(s, None, None))
}
fn summary(body: &str) -> Comment {
    Comment {
        id: "summary-1".into(),
        author: "chatgpt-codex-connector[bot]".into(),
        body: body.into(),
        updated_at: "2026-09-07T16:30:00Z".into(),
    }
}

#[test]
fn no_reviewers_is_unknown() {
    assert_eq!(state(&snapshot()), State::Unknown);
}
#[test]
fn cubic_zero_issues_is_clean_even_without_formal_approval() {
    let mut s = snapshot();
    s.reviews.push(review(
        Agent::Cubic,
        include_str!("fixtures/cubic-clean.md"),
    ));
    assert_eq!(state(&s), State::Approved);
}
#[test]
fn cubic_no_issues_and_addressed_summaries() {
    for body in [
        "No issues found across 3 files",
        "All reported issues were addressed across 3 files",
    ] {
        let mut s = snapshot();
        s.reviews.push(review(Agent::Cubic, body));
        assert_eq!(state(&s), State::Approved);
    }
}
#[test]
fn waits_for_every_reviewer_before_comments() {
    let mut s = snapshot();
    s.reviews.push(review(Agent::Cubic, "2 issues found"));
    s.threads.push(thread());
    s.reactions.push(eyes());
    assert_eq!(state(&s), State::Reviewing);
    s.reactions.clear();
    s.reviews.push(review(Agent::Codex, "Codex Review"));
    assert_eq!(state(&s), State::Comments);
}
#[test]
fn unconfirmed_expected_reviewer_blocks_comments() {
    let mut s = snapshot();
    s.reviews.push(review(Agent::Cubic, "2 issues found"));
    s.threads.push(thread());
    let agents = reviewers::evaluate(&s, None, Some(&[Agent::Cubic, Agent::Codex]));
    assert_eq!(reviewers::aggregate(&s, &agents), State::Unknown);
}
#[test]
fn unresolved_threads_override_clean_verdicts() {
    let mut s = snapshot();
    s.reviews.push(review(Agent::Cubic, "0 issues found"));
    s.threads.push(thread());
    assert_eq!(state(&s), State::Comments);
}
#[test]
fn resolving_threads_does_not_invent_approval() {
    let mut s = snapshot();
    s.reviews.push(review(Agent::Cubic, "2 issues found"));
    s.threads.push(Thread {
        resolved: true,
        ..thread()
    });
    assert_eq!(state(&s), State::Unknown);
}
#[test]
fn new_commit_invalidates_old_approval() {
    let mut s = snapshot();
    s.reviews.push(review(Agent::Cubic, "0 issues found"));
    s.head = "1234567890123456789012345678901234567890".into();
    assert_eq!(state(&s), State::Unknown);
}
#[test]
fn check_rerun_does_not_reuse_older_review() {
    let mut s = snapshot();
    s.reviews.push(review(Agent::Cubic, "0 issues found"));
    s.checks.push(Check {
        started_at: "2026-09-07T17:00:00Z".into(),
        ..check("completed")
    });
    assert_eq!(state(&s), State::Unknown);
    s.checks[0].status = "in_progress".into();
    assert_eq!(state(&s), State::Reviewing);
}
#[test]
fn failed_or_skipped_checks_are_not_clean() {
    for conclusion in ["failure", "timed_out", "cancelled", "skipped"] {
        let mut s = snapshot();
        s.reviews.push(review(Agent::Cubic, "0 issues found"));
        s.checks.push(Check {
            conclusion: conclusion.into(),
            ..check("completed")
        });
        assert_eq!(state(&s), State::Unknown);
    }
}
#[test]
fn latest_check_supersedes_old_run() {
    let mut s = snapshot();
    s.checks.push(check("in_progress"));
    s.checks.push(Check {
        id: "101".into(),
        started_at: "2026-09-07T17:00:00Z".into(),
        summary: "0 issues found".into(),
        ..check("completed")
    });
    assert_eq!(state(&s), State::Approved);
}
#[test]
fn cubic_check_summary_can_supply_verdict() {
    let mut s = snapshot();
    s.checks.push(Check {
        summary: "AI review completed with 2 reviews. 0 issues found across 3 files.".into(),
        ..check("completed")
    });
    assert_eq!(state(&s), State::Approved);
}
#[test]
fn humans_cannot_supply_bot_reactions() {
    let mut s = snapshot();
    s.reactions.push(Reaction {
        author: "someone".into(),
        ..eyes()
    });
    assert_eq!(state(&s), State::Unknown);
}
#[test]
fn cold_start_thumb_is_unknown() {
    let mut s = snapshot();
    s.reactions.push(thumb());
    assert_eq!(state(&s), State::Unknown);
}
#[test]
fn running_to_new_thumb_can_approve_same_commit() {
    let mut s = snapshot();
    s.reactions.push(eyes());
    let previous = transition(s.clone(), None, None, 100, 0);
    s.reactions = vec![thumb()];
    let clean = transition(s.clone(), Some(&previous), None, 130, 0);
    assert_eq!(clean.state, State::Approved);
    assert_eq!(
        transition(s, Some(&clean), None, 160, 0).state,
        State::Approved
    );
}
#[test]
fn thumb_before_running_does_not_finish_rerun() {
    let mut s = snapshot();
    s.reactions = vec![eyes(), thumb()];
    let previous = transition(s.clone(), None, None, 100, 0);
    s.reactions = vec![thumb()];
    assert_eq!(
        transition(s, Some(&previous), None, 130, 0).state,
        State::Unknown
    );
}
#[test]
fn changed_head_does_not_bind_new_thumb_to_old_run() {
    let mut s = snapshot();
    s.reactions.push(eyes());
    let previous = transition(s.clone(), None, None, 100, 0);
    s.head = "1234567890123456789012345678901234567890".into();
    s.reactions = vec![thumb()];
    assert_eq!(
        transition(s, Some(&previous), None, 130, 0).state,
        State::Unknown
    );
}
#[test]
fn codex_summary_running_overrides_findings() {
    let mut s = snapshot();
    s.comments
        .push(summary(include_str!("fixtures/codex-running.md")));
    s.reviews.push(review(Agent::Codex, "Findings"));
    s.threads.push(thread());
    assert_eq!(state(&s), State::Reviewing);
}
#[test]
fn codex_completed_summary_binds_clean_result() {
    let mut s = snapshot();
    s.comments.push(summary("<!-- codex-pull-request-review-summary -->\n| Code Review | Completed — no findings | `abcdef0` | New commits |"));
    assert_eq!(state(&s), State::Approved);
}
#[test]
fn codex_security_run_must_finish_too() {
    let mut s = snapshot();
    s.comments.push(summary("<!-- codex-pull-request-review-summary -->\n| Code Review | Completed — no findings | `abcdef0` | New commits |\n| Security Review | Running | `abcdef0` | Manual |"));
    assert_eq!(state(&s), State::Reviewing);
}
#[test]
fn summary_for_old_commit_does_not_approve() {
    let mut s = snapshot();
    s.comments.push(summary("<!-- codex-pull-request-review-summary -->\n| Code Review | Completed — no findings | `1111111` | New commits |"));
    s.reactions.push(thumb());
    assert_eq!(state(&s), State::Unknown);
}
#[test]
fn codex_error_summary_blocks_old_review() {
    let mut s = snapshot();
    s.comments.push(summary("<!-- codex-pull-request-review-summary -->\n| Code Review | Failed | `abcdef0` | New commits |"));
    s.reviews.push(Review {
        state: "APPROVED".into(),
        ..review(Agent::Codex, "")
    });
    assert_eq!(state(&s), State::Unknown);
}
#[test]
fn unrelated_successful_ci_does_not_count() {
    let mut s = snapshot();
    s.checks.push(Check {
        app: "github-actions".into(),
        summary: "0 issues found".into(),
        ..check("completed")
    });
    assert_eq!(state(&s), State::Unknown);
}
#[test]
fn coderabbit_success_without_verdict_is_unknown() {
    let mut s = snapshot();
    s.checks.push(Check {
        app: "coderabbitai".into(),
        ..check("completed")
    });
    assert_eq!(state(&s), State::Unknown);
}
#[test]
fn coderabbit_explicit_counts() {
    for (body, expected) in [
        ("**Actionable comments posted: 0**", State::Approved),
        ("**Actionable comments posted: 3**", State::Comments),
    ] {
        let mut s = snapshot();
        s.reviews.push(review(Agent::CodeRabbit, body));
        if expected == State::Comments {
            s.threads.push(thread());
        }
        assert_eq!(state(&s), expected);
    }
}
#[test]
fn repository_override_replaces_inference() {
    let mut s = snapshot();
    s.reactions.push(eyes());
    s.reviews.push(review(Agent::Cubic, "0 issues found"));
    let agents = reviewers::evaluate(&s, None, Some(&[Agent::Cubic]));
    assert_eq!(reviewers::aggregate(&s, &agents), State::Approved);
}
#[test]
fn final_results_must_settle_and_restart_reconfirms() {
    let mut s = snapshot();
    s.reviews.push(review(Agent::Cubic, "0 issues found"));
    let first = transition(s.clone(), None, None, 100, 30);
    assert_eq!(first.state, State::Reviewing);
    let confirmed = transition(s.clone(), Some(&first), None, 130, 30);
    assert_eq!(confirmed.state, State::Approved);
    let stale = PullRequest {
        stale: true,
        ..confirmed
    };
    assert_eq!(
        transition(s, Some(&stale), None, 200, 30).state,
        State::Reviewing
    );
}
#[test]
fn comment_change_resets_acknowledgement_without_status_change() {
    let mut s = snapshot();
    s.reviews.push(review(Agent::Cubic, "2 issues found"));
    s.threads.push(thread());
    let mut first = transition(s.clone(), None, None, 100, 0);
    first.acknowledged = Some(first.update_id.clone());
    assert!(!first.needs_attention());
    s.threads[0].last_comment_id = "comment-2".into();
    let next = transition(s, Some(&first), None, 130, 0);
    assert_eq!(next.state, State::Comments);
    assert!(next.needs_attention());
}
#[test]
fn title_change_does_not_reset_acknowledgement() {
    let mut s = snapshot();
    s.reviews.push(review(Agent::Cubic, "0 issues found"));
    let mut first = transition(s.clone(), None, None, 100, 0);
    first.acknowledged = Some(first.update_id.clone());
    s.title = "New title".into();
    assert!(!transition(s, Some(&first), None, 130, 0).needs_attention());
}
#[test]
fn sqlite_persists_ack_and_deduplicates_notifications() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut s = snapshot();
    s.reviews.push(review(Agent::Cubic, "0 issues found"));
    let mut pr = transition(s, None, None, 100, 0);
    store.save(&pr).unwrap();
    let notification = store.notification(&pr).unwrap().unwrap();
    store.mark_delivered(&notification).unwrap();
    assert!(store.notification(&pr).unwrap().is_none());
    let update = pr.update_id.clone();
    store.acknowledge(&mut pr, &update, true).unwrap();
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    let restored = store.load().unwrap().remove(0);
    assert_eq!(restored.acknowledged, Some(update));
    assert!(restored.stale);
    assert!(store.notification_target(&notification).unwrap().is_some());
}
#[test]
fn old_notification_cannot_ack_newer_update() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut s = snapshot();
    s.reviews.push(review(Agent::Cubic, "2 issues found"));
    s.threads.push(thread());
    let first = transition(s.clone(), None, None, 100, 0);
    let notification = store.notification(&first).unwrap().unwrap();
    s.threads[0].last_comment_id = "comment-2".into();
    let mut next = transition(s, Some(&first), None, 130, 0);
    let (_, old_update, _) = store.notification_target(&notification).unwrap().unwrap();
    assert!(!store.acknowledge(&mut next, &old_update, true).unwrap());
    assert!(next.needs_attention());
}
#[test]
fn changing_accounts_clears_cached_attention() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    assert!(!store.set_viewer("one").unwrap());
    store
        .save(&transition(snapshot(), None, None, 100, 0))
        .unwrap();
    assert!(store.set_viewer("two").unwrap());
    assert!(store.load().unwrap().is_empty());
}
#[test]
fn stale_cache_never_affects_main_icon() {
    let mut s = snapshot();
    s.reviews.push(review(Agent::Cubic, "0 issues found"));
    let mut pr = transition(s, None, None, 100, 0);
    assert!(pr.needs_attention());
    pr.stale = true;
    assert!(!pr.needs_attention());
}

#[test]
fn subscription_limited_agent_does_not_block_inferred_reviewers() {
    let mut s = snapshot();
    s.checks.push(Check {
        conclusion: "neutral".into(),
        summary: "You've reached your plan's monthly review limit.".into(),
        ..check("completed")
    });
    s.reviews.push(review(Agent::Codex, "Findings"));
    s.threads.push(thread());
    assert_eq!(state(&s), State::Comments);
    let agents = reviewers::evaluate(&s, None, Some(&[Agent::Codex, Agent::Cubic]));
    assert_eq!(reviewers::aggregate(&s, &agents), State::Unknown);
}

#[test]
fn all_skipped_does_not_approve() {
    let mut s = snapshot();
    s.checks.push(Check {
        conclusion: "neutral".into(),
        summary: "You've reached your plan's monthly review limit.".into(),
        ..check("completed")
    });
    assert_eq!(state(&s), State::Unknown);
}

#[test]
fn codex_late_thumb_completes_observed_run() {
    let mut s = snapshot();
    s.reactions.push(eyes());
    let running = transition(s.clone(), None, None, 100, 0);
    s.reactions.clear();
    let gap = transition(s.clone(), Some(&running), None, 130, 0);
    assert_eq!(gap.state, State::Unknown);
    s.reactions.push(thumb());
    assert_eq!(
        transition(s, Some(&gap), None, 160, 0).state,
        State::Approved
    );
}

#[test]
fn missing_rerun_result_cannot_restore_old_approval_on_later_polls() {
    let mut s = snapshot();
    s.reactions.push(eyes());
    s.reviews.push(Review {
        state: "APPROVED".into(),
        ..review(Agent::Codex, "")
    });
    let running = transition(s.clone(), None, None, 100, 0);
    s.reactions.clear();
    let gap = transition(s.clone(), Some(&running), None, 130, 0);
    assert_eq!(gap.state, State::Unknown);
    assert_eq!(
        transition(s, Some(&gap), None, 160, 0).state,
        State::Unknown
    );
}

#[test]
fn persisted_reaction_association_survives_restart() {
    let mut s = snapshot();
    s.reactions.push(eyes());
    let running = transition(s.clone(), None, None, 100, 0);
    s.reactions = vec![thumb()];
    let mut clean = transition(s.clone(), Some(&running), None, 130, 0);
    clean.stale = true;
    assert_eq!(
        transition(s, Some(&clean), None, 160, 0).state,
        State::Approved
    );
}
