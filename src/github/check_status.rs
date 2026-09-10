//! Summarize all CI checks using responses already fetched for reviewer detection.
use super::*;

pub(super) fn check_run(check: &Value) -> CheckState {
    if check["status"] != "completed" {
        return CheckState::Running;
    }
    match check["conclusion"].as_str() {
        Some("success" | "neutral" | "skipped") => CheckState::Green,
        _ => CheckState::Failed,
    }
}

pub(super) fn commit_statuses(statuses: &[Value]) -> CheckState {
    // This endpoint includes historical results. Only the latest status for each
    // case-insensitive context contributes, regardless of author or pagination.
    let mut latest: BTreeMap<String, &Value> = BTreeMap::new();
    for status in statuses {
        let context = string(status, "context").to_lowercase();
        let current = latest.entry(context).or_insert(status);
        if status["id"].as_u64() > current["id"].as_u64() {
            *current = status;
        }
    }
    latest
        .values()
        .map(|status| match status["state"].as_str() {
            Some("success") => CheckState::Green,
            Some("pending") => CheckState::Running,
            _ => CheckState::Failed,
        })
        .max()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_and_terminal_check_states() {
        for status in ["queued", "in_progress", "requested", "waiting", "pending"] {
            assert_eq!(
                check_run(&json!({"status":status,"conclusion":null})),
                CheckState::Running
            );
        }
        for conclusion in ["success", "neutral", "skipped"] {
            assert_eq!(
                check_run(&json!({"status":"completed","conclusion":conclusion})),
                CheckState::Green
            );
        }
        for conclusion in [
            "action_required",
            "cancelled",
            "timed_out",
            "failure",
            "stale",
            "startup_failure",
        ] {
            assert_eq!(
                check_run(&json!({"status":"completed","conclusion":conclusion})),
                CheckState::Failed
            );
        }
        assert_eq!(
            CheckState::Running.max(CheckState::Failed),
            CheckState::Failed
        );
        assert_eq!(
            CheckState::Green.max(CheckState::Running),
            CheckState::Running
        );
    }

    #[test]
    fn legacy_contexts_use_only_the_latest_result() {
        let mut statuses = vec![
            json!({"id":4,"context":"deploy","state":"success"}),
            json!({"id":2,"context":"tests","state":"success"}),
            json!({"id":3,"context":"DEPLOY","state":"pending"}),
            json!({"id":1,"context":"tests","state":"failure"}),
        ];
        assert_eq!(commit_statuses(&statuses), CheckState::Green);
        statuses.reverse();
        assert_eq!(commit_statuses(&statuses), CheckState::Green);
        statuses.push(json!({"id":5,"context":"tests","state":"pending"}));
        assert_eq!(commit_statuses(&statuses), CheckState::Running);
        statuses.push(json!({"id":6,"context":"deploy","state":"error"}));
        assert_eq!(commit_statuses(&statuses), CheckState::Failed);
        assert_eq!(commit_statuses(&[]), CheckState::Green);
    }
}
