use gopher::{model::*, reviewers, store::Store, worker::transition};

const HEAD: &str = "abcdef0123456789012345678901234567890123";

#[test]
fn reviewing_elapsed_time_floors_minutes_and_does_not_change_update_identity() {
    let mut s = snapshot();
    s.reactions.push(eyes());
    let first = transition(s.clone(), None, None, 1000, 0);
    assert_eq!(first.reviewing_since, Some(1000));
    for (seconds, label) in [
        (-10, "Reviewing (0m)"),
        (0, "Reviewing (0m)"),
        (59, "Reviewing (0m)"),
        (165, "Reviewing (2m)"),
        (3599, "Reviewing (59m)"),
        (3600, "Reviewing (1h 0m)"),
        (3765, "Reviewing (1h 2m)"),
    ] {
        assert_eq!(first.status_label(1000 + seconds), label);
    }
    let later = transition(s, Some(&first), None, 1200, 0);
    assert_eq!(later.reviewing_since, first.reviewing_since);
    assert_eq!(later.update_id, first.update_id);
}

#[test]
fn reviewing_clock_survives_restart_and_resets_for_new_review_or_commit() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let mut s = snapshot();
    s.reactions.push(eyes());
    let first = transition(s.clone(), None, None, 1000, 0);
    store.save(&first).unwrap();
    let restored = store.load().unwrap().remove(0);
    assert!(restored.stale);
    assert_eq!(restored.status_label(1200), "Unknown");
    let resumed = transition(s.clone(), Some(&restored), None, 1200, 0);
    assert_eq!(resumed.reviewing_since, Some(1000));

    s.head = "new-commit".into();
    let next_commit = transition(s.clone(), Some(&resumed), None, 1300, 0);
    assert_eq!(next_commit.reviewing_since, Some(1300));
    s.reactions.clear();
    let finished = transition(s.clone(), Some(&next_commit), None, 1400, 0);
    assert_ne!(finished.state, State::Reviewing);
    assert_eq!(finished.reviewing_since, None);
    s.reactions.push(eyes());
    let rerun = transition(s, Some(&finished), None, 1500, 0);
    assert_eq!(rerun.reviewing_since, Some(1500));
}

#[test]
fn legacy_cache_starts_review_clock_on_next_observation() {
    let mut s = snapshot();
    s.reactions.push(eyes());
    let mut data = serde_json::to_value(transition(s.clone(), None, None, 1000, 0)).unwrap();
    data.as_object_mut().unwrap().remove("reviewing_since");
    let legacy: PullRequest = serde_json::from_value(data).unwrap();
    assert_eq!(legacy.reviewing_since, None);
    let refreshed = transition(s, Some(&legacy), None, 2000, 0);
    assert_eq!(refreshed.reviewing_since, Some(2000));
}

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
fn no_reviewers_have_no_raw_review_verdict() {
    assert_eq!(state(&snapshot()), State::Unknown);
}
#[test]
fn fresh_pr_without_review_activity_is_ready_for_review() {
    let first = transition(snapshot(), None, None, 100, 30);
    assert_eq!(first.state, State::ReadyForReview);
    assert_eq!(first.status_label(100), "Ready for review");
    assert!(!first.needs_attention());
    let next = transition(snapshot(), Some(&first), None, 130, 30);
    assert_eq!(next.state, State::ReadyForReview);
    assert_eq!(next.update_id, first.update_id);

    let required = transition(snapshot(), None, Some(&[Agent::Codex]), 100, 30);
    assert_eq!(required.state, State::ReadyForReview);
    assert_eq!(required.agents[0].verdict, Verdict::Unknown);

    let no_automated_reviewers = transition(snapshot(), None, Some(&[]), 100, 30);
    assert_eq!(no_automated_reviewers.state, State::Unknown);
    let mut unrelated_activity = snapshot();
    unrelated_activity
        .reviews
        .push(review(Agent::Cubic, "0 issues found"));
    let codex_only = transition(unrelated_activity, None, Some(&[Agent::Codex]), 100, 30);
    assert_eq!(codex_only.state, State::ReadyForReview);
}
#[test]
fn review_activity_or_lost_participation_is_not_ready_for_review() {
    let mut s = snapshot();
    s.reviews.push(review(Agent::Cubic, "0 issues found"));
    let approved = transition(s.clone(), None, None, 100, 0);
    assert_eq!(approved.state, State::Approved);
    s.reviews.clear();
    let missing = transition(s.clone(), Some(&approved), None, 130, 0);
    assert_eq!(missing.state, State::Unknown);

    let mut old_review = review(Agent::Cubic, "0 issues found");
    old_review.commit = "older-commit".into();
    s.reviews.push(old_review);
    assert_eq!(transition(s, None, None, 160, 0).state, State::Unknown);
}
#[test]
fn draft_or_unresolved_threads_are_not_ready_for_review() {
    let mut s = snapshot();
    s.draft = true;
    assert_eq!(
        transition(s.clone(), None, None, 100, 0).state,
        State::Unknown
    );
    s.draft = false;
    s.threads.push(thread());
    assert_eq!(transition(s, None, None, 100, 0).state, State::Unknown);
}
#[test]
fn explicitly_skipped_reviewers_do_not_block_ready_for_review() {
    let mut s = snapshot();
    s.draft = true;
    s.checks.push(skipped_coderabbit());
    let draft = transition(s.clone(), None, None, 100, 0);
    assert_eq!(draft.state, State::Unknown);

    s.draft = false;
    let ready = transition(s.clone(), Some(&draft), None, 130, 30);
    assert_eq!(ready.state, State::ReadyForReview);
    assert!(!ready.needs_attention());
    let coderabbit = &ready.agents[0];
    assert_eq!(coderabbit.verdict, Verdict::Skipped);
    assert_eq!(
        coderabbit.reason,
        "Review skipped — automatic reviews are disabled"
    );

    s.reactions.push(eyes());
    let reviewing = transition(s.clone(), Some(&ready), None, 160, 30);
    assert_eq!(reviewing.state, State::Reviewing);

    s.reactions.clear();
    let required = transition(
        s.clone(),
        Some(&draft),
        Some(&[Agent::Codex, Agent::CodeRabbit]),
        130,
        30,
    );
    assert_eq!(required.state, State::Unknown);
    let codex_only = transition(s, Some(&draft), Some(&[Agent::Codex]), 130, 30);
    assert_eq!(codex_only.state, State::ReadyForReview);
}
#[test]
fn review_after_an_explicit_skip_replaces_the_skip() {
    let mut s = snapshot();
    s.checks.push(skipped_coderabbit());
    let mut manual = review(Agent::CodeRabbit, "No actionable comments were generated");
    manual.submitted_at = "2026-09-07T16:20:00Z".into(); // Before the skip check started.
    s.reviews.push(manual);
    assert_eq!(
        reviewers::evaluate(&s, None, None)[0].verdict,
        Verdict::Skipped
    );
    assert_eq!(
        transition(s.clone(), None, None, 100, 0).state,
        State::ReadyForReview
    );

    s.reviews[0].submitted_at = "2026-09-07T16:30:00Z".into();
    assert_eq!(
        reviewers::evaluate(&s, None, None)[0].verdict,
        Verdict::Clean
    );
    assert_eq!(
        transition(s.clone(), None, None, 100, 0).state,
        State::Approved
    );
    s.reviews[0].body = "Actionable comments posted: 2".into();
    s.threads.push(Thread {
        author: "coderabbitai[bot]".into(),
        resolved: true,
        ..thread()
    });
    // Resolved findings are reviewed, not waiting for a first review.
    assert_eq!(transition(s, None, None, 100, 0).state, State::Unknown);
}
fn usage_limit(updated_at: &str) -> Comment {
    Comment {
        id: "limit-1".into(),
        updated_at: updated_at.into(),
        ..summary(include_str!("fixtures/codex-usage-limit.md"))
    }
}
#[test]
fn codex_usage_limit_is_an_explicit_skip_until_newer_codex_activity() {
    let mut s = snapshot();
    s.checks.push(skipped_coderabbit());
    s.comments.push(usage_limit("2026-09-07T16:35:00Z"));
    let agents = reviewers::evaluate(&s, None, None);
    let codex = agents.iter().find(|a| a.agent == Agent::Codex).unwrap();
    assert_eq!(codex.verdict, Verdict::Skipped);
    assert_eq!(codex.run_id, "limit-1");
    assert_eq!(codex.reason, "Review skipped — Codex usage limit reached");
    assert_eq!(
        transition(s.clone(), None, None, 100, 0).state,
        State::ReadyForReview
    );
    let required = reviewers::evaluate(&s, None, Some(&[Agent::Codex]));
    assert_eq!(required[0].verdict, Verdict::Unknown);
    assert!(required[0].reason.contains("Required reviewer did not run"));

    // An older summary or review does not hide a newer notice.
    let clean = "<!-- codex-pull-request-review-summary -->\n| Code Review | Completed — no findings | `abcdef0` | New commits |";
    s.comments.push(summary(clean));
    s.reviews.push(Review {
        state: "APPROVED".into(),
        ..review(Agent::Codex, "")
    });
    assert_eq!(state(&s), State::Unknown);

    // Codex activity after the notice takes over again.
    s.reactions.push(Reaction {
        created_at: "2026-09-07T16:40:00Z".into(),
        ..thumb()
    });
    assert_ne!(
        reviewers::evaluate(&s, None, None)
            .iter()
            .find(|a| a.agent == Agent::Codex)
            .unwrap()
            .verdict,
        Verdict::Skipped
    );
    s.reactions.clear();
    s.reviews[0].submitted_at = "2026-09-07T16:40:00Z".into();
    assert_eq!(state(&s), State::Approved);
    s.reviews.clear();
    s.comments[1].updated_at = "2026-09-07T16:40:00Z".into();
    assert_eq!(state(&s), State::Approved);
    s.comments.truncate(1);

    // A leftover eyes reaction from before the notice does not keep it running.
    s.reactions.push(eyes());
    assert_eq!(state(&s), State::Unknown);
    s.reactions[0].created_at = "2026-09-07T16:40:00Z".into();
    assert_eq!(state(&s), State::Reviewing);
}
#[test]
fn codex_usage_limit_tied_with_other_activity_is_not_a_skip() {
    let codex = |s: &Snapshot| {
        reviewers::evaluate(s, None, None)
            .into_iter()
            .find(|a| a.agent == Agent::Codex)
            .unwrap()
            .verdict
    };
    let tie = "2026-09-07T16:30:00Z";
    let mut s = snapshot();
    s.comments.push(usage_limit(tie));
    assert_eq!(codex(&s), Verdict::Skipped);
    // A second, newer notice is not activity that clears the skip.
    s.comments.push(Comment {
        id: "limit-2".into(),
        ..usage_limit("2026-09-07T16:31:00Z")
    });
    assert_eq!(codex(&s), Verdict::Skipped);
    s.comments.pop();

    let mut summary_tie = s.clone();
    summary_tie
        .comments
        .push(summary(include_str!("fixtures/codex-running.md")));
    assert_eq!(codex(&summary_tie), Verdict::Running);
    let mut review_tie = s.clone();
    review_tie.reviews.push(Review {
        state: "APPROVED".into(),
        submitted_at: tie.into(),
        ..review(Agent::Codex, "")
    });
    assert_eq!(codex(&review_tie), Verdict::Clean);
    s.reactions.push(Reaction {
        created_at: tie.into(),
        ..eyes()
    });
    assert_eq!(codex(&s), Verdict::Running);
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
fn explicit_codex_failure_blocks_old_approval() {
    let mut s = snapshot();
    s.comments.push(summary("<!-- codex-pull-request-review-summary -->\n| Code Review | Failed | `abcdef0` | New commits |"));
    s.reviews.push(Review {
        state: "APPROVED".into(),
        ..review(Agent::Codex, "")
    });
    assert_eq!(state(&s), State::Failed);
}
#[test]
fn codex_failed_summary_settles_and_notifies_once_per_update() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let mut s = snapshot();
    s.comments
        .push(summary(include_str!("fixtures/codex-failed.md")));

    let first = transition(s.clone(), None, None, 100, 30);
    assert_eq!(first.agents[0].verdict, Verdict::Failed);
    assert_eq!(first.state, State::Reviewing);
    assert!(!first.needs_attention());
    let mut confirmed = transition(s.clone(), Some(&first), None, 130, 30);
    assert_eq!(confirmed.state, State::Failed);
    assert!(confirmed.needs_attention());
    let notification = store.notification(&confirmed).unwrap().unwrap();
    store.mark_delivered(&notification).unwrap();
    assert!(store.notification(&confirmed).unwrap().is_none());

    confirmed.acknowledged = Some(confirmed.update_id.clone());
    let stable = transition(s.clone(), Some(&confirmed), None, 160, 30);
    assert_eq!(stable.update_id, confirmed.update_id);
    assert!(!stable.needs_attention());

    s.comments[0].body = s.comments[0].body.replace("10:23:41", "10:25:41");
    let rerun = transition(s, Some(&stable), None, 190, 30);
    assert_ne!(rerun.update_id, stable.update_id);
    assert_eq!(rerun.state, State::Reviewing);
    let failed_again = transition(rerun.snapshot.clone(), Some(&rerun), None, 220, 30);
    assert_eq!(failed_again.state, State::Failed);
    assert!(failed_again.needs_attention());
    assert!(store.notification(&failed_again).unwrap().is_some());
}
#[test]
fn codex_failure_requires_current_commit_and_is_distinct_from_other_statuses() {
    for (status, expected) in [
        ("Failed", State::Failed),
        ("Not failed", State::Unknown),
        ("Cancelled", State::Unknown),
        ("Skipped", State::Unknown),
        ("Error", State::Unknown),
    ] {
        let mut s = snapshot();
        s.comments.push(summary(&format!("<!-- codex-pull-request-review-summary -->\n| Code Review | {status} | `abcdef0` | New commits |")));
        assert_eq!(state(&s), expected, "{status}");
        s.head = "1111111000000000000000000000000000000000".into();
        assert_eq!(state(&s), State::Unknown, "old {status}");
    }
}
#[test]
fn current_codex_failure_is_not_hidden_by_an_older_summary_row() {
    let mut s = snapshot();
    s.comments.push(summary("<!-- codex-pull-request-review-summary -->\n| Code Review | Running | `1111111` | New commits |\n| Security Review | Failed | `abcdef0` | Manual |"));
    assert_eq!(state(&s), State::Failed);
    s.comments[0].body = s.comments[0]
        .body
        .replace("Failed", "Completed — no findings");
    assert_eq!(state(&s), State::Unknown);
}
#[test]
fn running_review_precedes_failure_but_failure_precedes_missing_evidence() {
    let mut s = snapshot();
    s.comments.push(summary("<!-- codex-pull-request-review-summary -->\n| Code Review | Failed | `abcdef0` | New commits |"));
    let failed = reviewers::evaluate(&s, None, None);
    let missing = AgentResult {
        agent: Agent::CodeRabbit,
        verdict: Verdict::Unknown,
        run_id: String::new(),
        reason: String::new(),
    };
    assert_eq!(
        reviewers::aggregate(&s, &[failed[0].clone(), missing.clone()]),
        State::Failed
    );
    let running = AgentResult {
        verdict: Verdict::Running,
        ..missing
    };
    assert_eq!(
        reviewers::aggregate(&s, &[failed[0].clone(), running]),
        State::Reviewing
    );
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
fn draft_to_ready_creates_a_durable_actionable_update() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let mut s = snapshot();
    s.draft = true;
    s.reviews.push(review(Agent::Cubic, "0 issues found"));
    let mut draft = transition(s.clone(), None, None, 100, 0);
    assert_eq!(draft.state, State::Approved);
    draft.acknowledged = Some(draft.update_id.clone());

    s.draft = false;
    let ready = transition(s.clone(), Some(&draft), None, 130, 0);
    assert_eq!(ready.ready_generation, 1);
    assert_ne!(ready.update_id, draft.update_id);
    assert!(ready.needs_attention());
    assert_eq!(
        transition(s.clone(), Some(&ready), None, 160, 0).update_id,
        ready.update_id
    );

    store.save(&ready).unwrap();
    let restored = store.load().unwrap().remove(0);
    assert_eq!(restored.ready_generation, 1);
    let notice = store.notification(&ready).unwrap().unwrap();
    assert!(store.notification_target(&notice).unwrap().is_some());

    s.draft = true;
    let again_draft = transition(s.clone(), Some(&restored), None, 190, 0);
    assert_eq!(again_draft.update_id, ready.update_id);
    s.draft = false;
    let again_ready = transition(s, Some(&again_draft), None, 220, 0);
    assert_eq!(again_ready.ready_generation, 2);
    assert_ne!(again_ready.update_id, ready.update_id);
}

#[test]
fn ready_transition_waits_for_settled_evidence_after_stale_restore() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let mut snapshot = snapshot();
    snapshot.draft = true;
    snapshot
        .reviews
        .push(review(Agent::Cubic, "0 issues found"));
    let reviewing = transition(snapshot.clone(), None, None, 100, 30);
    let mut approved = transition(snapshot.clone(), Some(&reviewing), None, 140, 30);
    assert_eq!(approved.state, State::Approved);
    approved.acknowledged = Some(approved.update_id.clone());
    store.save(&approved).unwrap();

    let restored = store.load().unwrap().remove(0);
    assert!(restored.stale);
    snapshot.draft = false;
    let settling = transition(snapshot.clone(), Some(&restored), None, 200, 30);
    assert_eq!(settling.state, State::Reviewing);
    assert!(settling.ready_pending);
    assert_eq!(settling.ready_generation, 0);
    store.save(&settling).unwrap();
    let settling_restored = store.load().unwrap().remove(0);
    assert!(settling_restored.ready_pending);
    let settling_again = transition(snapshot.clone(), Some(&settling_restored), None, 240, 30);
    assert_eq!(settling_again.state, State::Reviewing);
    assert!(settling_again.ready_pending);

    let ready = transition(snapshot, Some(&settling_again), None, 280, 30);
    assert_eq!(ready.state, State::Approved);
    assert!(!ready.ready_pending);
    assert_eq!(ready.ready_generation, 1);
    assert_ne!(ready.update_id, approved.update_id);
    assert!(ready.needs_attention());
}
#[test]
fn labels_persist_without_resetting_acknowledgement_and_legacy_cache_still_loads() {
    let mut snapshot = snapshot();
    snapshot
        .reviews
        .push(review(Agent::Cubic, "0 issues found"));
    let mut previous = transition(snapshot.clone(), None, None, 100, 0);
    previous.acknowledged = Some(previous.update_id.clone());
    snapshot.labels.push(PrLabel {
        name: "bug".into(),
        color: "d73a4a".into(),
    });
    let current = transition(snapshot, Some(&previous), None, 130, 0);
    assert_eq!(current.update_id, previous.update_id);
    assert!(!current.needs_attention());
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.save(&current).unwrap();
    assert_eq!(
        store.load().unwrap()[0].snapshot.labels,
        current.snapshot.labels
    );
    let mut legacy = serde_json::to_value(&current).unwrap();
    legacy["snapshot"].as_object_mut().unwrap().remove("labels");
    let restored: PullRequest = serde_json::from_value(legacy).unwrap();
    assert!(restored.snapshot.labels.is_empty());
    assert_eq!(restored.acknowledged, previous.acknowledged);
}
#[test]
fn check_summary_persists_without_changing_reviews_or_acknowledgements() {
    let mut snapshot = snapshot();
    snapshot
        .reviews
        .push(review(Agent::Cubic, "0 issues found"));
    let mut previous = transition(snapshot.clone(), None, None, 100, 0);
    previous.acknowledged = Some(previous.update_id.clone());
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.save(&previous).unwrap();
    let notification = store.notification(&previous).unwrap().unwrap();
    store.mark_delivered(&notification).unwrap();
    for state in [
        CheckState::Running,
        CheckState::Failed,
        CheckState::Conflicts,
        CheckState::Green,
    ] {
        snapshot.check_state = Some(state);
        let current = transition(snapshot.clone(), Some(&previous), None, 130, 0);
        assert_eq!(current.state, State::Approved);
        assert_eq!(current.update_id, previous.update_id);
        assert_eq!(current.acknowledged, previous.acknowledged);
        assert!(!current.needs_attention());
        store.save(&current).unwrap();
        assert!(store.notification(&current).unwrap().is_none());
        assert_eq!(store.load().unwrap()[0].snapshot.check_state, Some(state));
    }
    let mut legacy = serde_json::to_value(&snapshot).unwrap();
    legacy.as_object_mut().unwrap().remove("check_state");
    let restored: Snapshot = serde_json::from_value(legacy).unwrap();
    assert_eq!(restored.check_state, None);
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

fn cubic_branch_rewrite() -> Check {
    Check {
        conclusion: "neutral".into(),
        summary: include_str!("fixtures/cubic-branch-rewrite.md").into(),
        ..check("completed")
    }
}

#[test]
fn cubic_branch_rewrite_is_an_explicit_skip_but_never_an_approval() {
    let mut s = snapshot();
    s.checks.push(cubic_branch_rewrite());
    let agents = reviewers::evaluate(&s, None, None);
    assert_eq!(agents[0].verdict, Verdict::Skipped);
    assert!(agents[0].reason.contains("branch history rewritten"));
    assert!(agents[0].reason.contains("manual review"));
    assert_eq!(state(&s), State::Unknown);

    s.reviews.push(Review {
        state: "APPROVED".into(),
        ..review(Agent::Codex, "")
    });
    assert_eq!(state(&s), State::Approved);
    s.threads.push(thread());
    assert_eq!(state(&s), State::Comments);
    let required = reviewers::evaluate(&s, None, Some(&[Agent::Codex, Agent::Cubic]));
    assert_eq!(reviewers::aggregate(&s, &required), State::Unknown);
    let cubic = required.iter().find(|a| a.agent == Agent::Cubic).unwrap();
    assert_eq!(cubic.verdict, Verdict::Unknown);
    assert!(cubic.reason.contains("Required reviewer did not run"));
}

#[test]
fn cubic_branch_rewrite_does_not_override_running_or_failed_checks() {
    for (status, conclusion, expected) in [
        ("in_progress", "neutral", Verdict::Running),
        ("completed", "failure", Verdict::Unknown),
    ] {
        let mut s = snapshot();
        s.checks.push(Check {
            status: status.into(),
            conclusion: conclusion.into(),
            ..cubic_branch_rewrite()
        });
        assert_eq!(reviewers::evaluate(&s, None, None)[0].verdict, expected);
    }
    // This observed wording belongs to Cubic, not another app with similar text.
    let mut s = snapshot();
    s.checks.push(Check {
        app: "coderabbitai".into(),
        ..cubic_branch_rewrite()
    });
    assert_eq!(
        reviewers::evaluate(&s, None, None)[0].verdict,
        Verdict::Unknown
    );
}

#[test]
fn manual_cubic_review_after_branch_rewrite_rejoins_participation() {
    let mut s = snapshot();
    s.checks.push(cubic_branch_rewrite());
    s.reviews.push(Review {
        state: "APPROVED".into(),
        ..review(Agent::Codex, "")
    });
    let skipped = transition(s.clone(), None, None, 100, 0);
    assert_eq!(skipped.state, State::Approved);
    s.checks.push(Check {
        id: "101".into(),
        ..check("in_progress")
    });
    let running = transition(s.clone(), Some(&skipped), None, 130, 0);
    assert_eq!(running.state, State::Reviewing);
    s.checks[1].status = "completed".into();
    let ambiguous = transition(s.clone(), Some(&running), None, 160, 0);
    assert_eq!(ambiguous.state, State::Unknown);
    s.checks[1].summary = "No issues found".into();
    let clean = transition(s, Some(&ambiguous), None, 190, 0);
    assert_eq!(clean.state, State::Approved);
    assert_eq!(
        clean
            .agents
            .iter()
            .find(|a| a.agent == Agent::Cubic)
            .unwrap()
            .verdict,
        Verdict::Clean
    );
}

fn skipped_coderabbit() -> Check {
    Check {
        app: "coderabbitai".into(),
        name: "CodeRabbit".into(),
        summary: "Review skipped: automatic reviews are disabled".into(),
        ..check("completed")
    }
}

#[test]
fn disappearing_skip_does_not_block_results_across_commits_polls_or_restart() {
    for findings in [false, true] {
        let mut s = snapshot();
        s.reviews.push(Review {
            state: "APPROVED".into(),
            ..review(Agent::Codex, "")
        });
        if findings {
            s.threads.push(thread());
        }
        s.checks.push(skipped_coderabbit());
        let skipped = transition(s.clone(), None, None, 100, 0);
        assert!(
            skipped
                .agents
                .iter()
                .any(|a| a.agent == Agent::CodeRabbit && a.verdict == Verdict::Skipped)
        );
        s.head = "new-head".into();
        s.reviews[0].commit = s.head.clone();
        s.checks.clear();
        let first = transition(s.clone(), Some(&skipped), None, 130, 30);
        assert_eq!(first.state, State::Reviewing); // Preserve the normal settling interval.
        let expected = if findings {
            State::Comments
        } else {
            State::Approved
        };
        let settled = transition(s.clone(), Some(&first), None, 160, 30);
        assert_eq!(settled.state, expected);
        assert!(!settled.agents.iter().any(|a| a.agent == Agent::CodeRabbit));
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.save(&settled).unwrap();
        let restored = store.load().unwrap().remove(0);
        assert_eq!(
            transition(s.clone(), Some(&restored), None, 190, 0).state,
            expected
        );

        // New activity must reintroduce the previously skipped reviewer immediately.
        s.checks.push(Check {
            status: "in_progress".into(),
            summary: String::new(),
            ..skipped_coderabbit()
        });
        assert_eq!(
            transition(s, Some(&settled), None, 200, 0).state,
            State::Reviewing
        );
    }
}

#[test]
fn explicitly_required_skipped_reviewer_still_blocks_after_its_check_disappears() {
    let mut s = snapshot();
    s.reviews.push(Review {
        state: "APPROVED".into(),
        ..review(Agent::Codex, "")
    });
    s.checks.push(skipped_coderabbit());
    let skipped = transition(s.clone(), None, None, 100, 0);
    s.checks.clear();
    assert_eq!(
        transition(
            s,
            Some(&skipped),
            Some(&[Agent::Codex, Agent::CodeRabbit]),
            130,
            0
        )
        .state,
        State::Unknown
    );
}

#[test]
fn missing_activity_from_actual_participants_still_blocks_with_a_history_reason() {
    for verdict in [
        Verdict::Running,
        Verdict::Clean,
        Verdict::Findings,
        Verdict::Unknown,
    ] {
        let mut s = snapshot();
        s.reviews.push(Review {
            state: "APPROVED".into(),
            ..review(Agent::Codex, "")
        });
        let mut previous = transition(s.clone(), None, None, 100, 0);
        previous.agents.push(AgentResult {
            agent: Agent::CodeRabbit,
            verdict,
            run_id: "previous-run".into(),
            reason: "Observed reviewer".into(),
        });
        let current = transition(s, Some(&previous), None, 130, 0);
        assert_eq!(current.state, State::Unknown);
        assert!(
            current
                .agents
                .iter()
                .find(|a| a.agent == Agent::CodeRabbit)
                .unwrap()
                .reason
                .starts_with("Previously participating reviewer")
        );
    }
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

#[test]
fn ignored_prs_survive_restart_pruning_and_account_changes() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store.set_viewer("one").unwrap();
    let pr = transition(snapshot(), None, None, 100, 0);
    store.save(&pr).unwrap();
    let notification = store.notification(&pr).unwrap().unwrap();
    assert_eq!(
        store.ignore(&pr.snapshot.id).unwrap(),
        vec![notification.clone()]
    );
    assert!(store.load().unwrap().is_empty());
    assert!(store.notification_target(&notification).unwrap().is_none());
    store.retain(&Default::default()).unwrap();
    store.set_viewer("two").unwrap();
    drop(store);
    let mut store = Store::open(dir.path()).unwrap();
    assert!(store.ignored().unwrap().contains(&pr.snapshot.id));
    assert!(store.ignore(&pr.snapshot.id).unwrap().is_empty());
    let archived = store.load_ignored().unwrap();
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].snapshot.repo, pr.snapshot.repo);
    assert_eq!(archived[0].snapshot.number, pr.snapshot.number);
    assert!(archived[0].stale);
    assert!(!archived[0].needs_attention());
    assert!(store.restore(&pr.snapshot.id).unwrap());
    // An identity lookup finishing after Restore cannot recreate the entry.
    store.save_ignored_details(&pr).unwrap();
    assert!(store.load_ignored().unwrap().is_empty());
    assert!(store.load().unwrap().is_empty());
    assert!(!store.restore(&pr.snapshot.id).unwrap());
    drop(store);
    assert!(
        Store::open(dir.path())
            .unwrap()
            .ignored()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn legacy_ignored_ids_can_be_listed_and_restored_without_network() {
    let dir = tempfile::tempdir().unwrap();
    let connection = rusqlite::Connection::open(dir.path().join("state.sqlite3")).unwrap();
    connection.execute_batch("CREATE TABLE ignored_prs(id TEXT PRIMARY KEY); INSERT INTO ignored_prs VALUES('legacy-id'); PRAGMA user_version=2;").unwrap();
    drop(connection);
    let mut store = Store::open(dir.path()).unwrap();
    let entries = store.load_ignored().unwrap();
    assert_eq!(entries[0].snapshot.id, "legacy-id");
    assert_eq!(entries[0].snapshot.number, 0);
    assert!(!entries[0].needs_attention());
    let known = PullRequest::unreviewed(Snapshot {
        id: "legacy-id".into(),
        repo: "owner/repo".into(),
        number: 42,
        open: true,
        ..Default::default()
    });
    store.save_ignored_details(&known).unwrap();
    assert_eq!(store.load_ignored().unwrap()[0].snapshot.number, 42);
    assert!(store.restore("legacy-id").unwrap());
    assert!(store.load_ignored().unwrap().is_empty());
}

#[test]
fn version_one_migration_preserves_acknowledgements_and_notifications() {
    let dir = tempfile::tempdir().unwrap();
    let mut pr = transition(snapshot(), None, None, 100, 0);
    pr.acknowledged = Some(pr.update_id.clone());
    let connection = rusqlite::Connection::open(dir.path().join("state.sqlite3")).unwrap();
    connection.execute_batch("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE prs (id TEXT PRIMARY KEY, data TEXT NOT NULL);
        CREATE TABLE notifications (id TEXT PRIMARY KEY, pr_id TEXT NOT NULL, update_id TEXT NOT NULL, url TEXT NOT NULL, delivered INTEGER NOT NULL DEFAULT 0);
        PRAGMA user_version=1;").unwrap();
    connection
        .execute(
            "INSERT INTO prs VALUES (?1,?2)",
            rusqlite::params![pr.snapshot.id, serde_json::to_string(&pr).unwrap()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO notifications VALUES ('notice',?1,?2,?3,1)",
            rusqlite::params![pr.snapshot.id, pr.update_id, pr.snapshot.url],
        )
        .unwrap();
    drop(connection);
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.load().unwrap()[0].acknowledged, pr.acknowledged);
    assert!(store.notification_target("notice").unwrap().is_some());
    assert!(store.ignored().unwrap().is_empty());
}

#[test]
fn notification_dismissal_targets_only_the_acknowledged_update() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut pr = transition(snapshot(), None, None, 100, 0);
    let first = store.notification(&pr).unwrap().unwrap();
    let old_update = pr.update_id.clone();
    pr.update_id = "new-update".into();
    let second = store.notification(&pr).unwrap().unwrap();
    assert_eq!(
        store
            .notification_ids(&pr.snapshot.id, Some(&old_update))
            .unwrap(),
        vec![first]
    );
    assert_eq!(
        store
            .notification_ids(&pr.snapshot.id, Some(&pr.update_id))
            .unwrap(),
        vec![second]
    );
}

#[test]
fn ignored_lifecycle_hides_closed_and_recovers_reopened_without_losing_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let mut pr = transition(snapshot(), None, None, 100, 0);
    pr.snapshot.title = "Original title".into();
    store.save(&pr).unwrap();
    store.ignore(&pr.snapshot.id).unwrap();
    let mut identity = pr.snapshot.clone();
    identity.open = false;
    identity.title = "Renamed title".into();
    identity.threads.clear();
    store.update_ignored_status(&identity).unwrap();
    assert!(store.load_ignored().unwrap().is_empty());
    assert!(store.ignored().unwrap().contains(&identity.id));
    drop(store);
    let mut store = Store::open(dir.path()).unwrap();
    assert!(store.load_ignored().unwrap().is_empty());
    identity.open = true;
    store.update_ignored_status(&identity).unwrap();
    let reopened = store.load_ignored().unwrap();
    assert_eq!(reopened.len(), 1);
    assert_eq!(reopened[0].snapshot.title, "Renamed title");
    assert_eq!(reopened[0].update_id, pr.update_id);
    assert_eq!(reopened[0].agents, pr.agents);
    assert!(!reopened[0].needs_attention());
    store.restore(&identity.id).unwrap();
    store.update_ignored_status(&identity).unwrap();
    assert!(store.load_ignored().unwrap().is_empty());
    assert!(store.ignored().unwrap().is_empty());
}
