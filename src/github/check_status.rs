//! Summarize CI checks, resolving Actions workflow identity only for ambiguous names.
use super::*;

#[derive(Clone, Debug)]
pub(super) struct WorkflowRun {
    workflow: u64,
    run: u64,
    event: String,
    branch: String,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Scope {
    Suite(u64),
    Workflow(u64, String, String),
    Execution(u64),
}

struct RankedCheck<'a> {
    order: (u64, u64),
    check: &'a Value,
}

pub(super) fn needs_workflow_identity(checks: &[Value]) -> bool {
    let mut suites = BTreeMap::new();
    for check in checks {
        if check["app"]["slug"] != "github-actions" {
            continue;
        }
        if let (Some(name), Some(suite)) =
            (check["name"].as_str(), check["check_suite"]["id"].as_u64())
            && *suites.entry(name).or_insert(suite) != suite
        {
            return true;
        }
    }
    false
}

pub(super) async fn workflow_runs(
    github: &Github,
    repo: &str,
    head: &str,
) -> Result<BTreeMap<u64, WorkflowRun>> {
    let mut workflows = BTreeMap::new();
    let mut page = 1;
    loop {
        let endpoint =
            format!("repos/{repo}/actions/runs?head_sha={head}&per_page=100&page={page}");
        let data = github
            .execute(&["api", "--hostname", "github.com", &endpoint], None)
            .await?;
        let runs = data["workflow_runs"]
            .as_array()
            .context("Missing workflow runs")?;
        for run in runs {
            if run["head_sha"] != head {
                continue;
            }
            if let (Some(suite), Some(workflow), Some(id), Some(event), Some(branch)) = (
                run["check_suite_id"].as_u64(),
                run["workflow_id"].as_u64(),
                run["id"].as_u64(),
                run["event"].as_str(),
                run["head_branch"].as_str(),
            ) {
                workflows.insert(
                    suite,
                    WorkflowRun {
                        workflow,
                        run: id,
                        event: event.into(),
                        branch: branch.into(),
                    },
                );
            }
        }
        if runs.len() < 100 {
            return Ok(workflows);
        }
        page += 1;
    }
}

pub(super) fn check_runs(checks: &[Value], workflows: &BTreeMap<u64, WorkflowRun>) -> CheckState {
    // Names alone do not establish replacement: independent Actions workflows
    // often both use `test`. Only verified workflow identity can join suites.
    let mut latest: BTreeMap<(String, Scope, &str), RankedCheck<'_>> = BTreeMap::new();
    let mut state = CheckState::Green;
    for check in checks {
        let app = check["app"]["id"]
            .as_u64()
            .map(|id| format!("id:{id}"))
            .or_else(|| {
                check["app"]["slug"]
                    .as_str()
                    .map(|slug| format!("slug:{slug}"))
            });
        let (Some(app), Some(name), Some(id), Some(suite)) = (
            app,
            check["name"].as_str(),
            check["id"].as_u64(),
            check["check_suite"]["id"].as_u64(),
        ) else {
            state = state.max(check_run(check));
            continue;
        };
        let workflow = (check["app"]["slug"] == "github-actions")
            .then(|| workflows.get(&suite))
            .flatten();
        let (scope, order) = match workflow {
            Some(run)
                if matches!(
                    run.event.as_str(),
                    "push" | "pull_request" | "pull_request_target"
                ) =>
            {
                (
                    Scope::Workflow(run.workflow, run.event.clone(), run.branch.clone()),
                    (run.run, id),
                )
            }
            // Automatic PR/push runs replace earlier results for that context.
            // Manual and other triggers may have different inputs: only jobs
            // within the same execution can replace one another there.
            Some(run) => (Scope::Execution(run.run), (run.run, id)),
            None => (Scope::Suite(suite), (0, id)),
        };
        // A late-created job from an older workflow run cannot replace the new run.
        let current = latest
            .entry((app, scope, name))
            .or_insert(RankedCheck { order, check });
        if order > current.order {
            *current = RankedCheck { order, check };
        }
    }
    latest
        .values()
        .fold(state, |state, ranked| state.max(check_run(ranked.check)))
}

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
    fn independent_executions_stay_blocking_but_automatic_runs_and_reruns_replace() {
        let checks = vec![
            json!({"id":100,"app":{"id":1,"slug":"github-actions"},"name":"test","status":"completed","conclusion":"failure","check_suite":{"id":10}}),
            json!({"id":200,"app":{"id":1,"slug":"github-actions"},"name":"test","status":"completed","conclusion":"success","check_suite":{"id":20}}),
        ];
        for event in [
            "push",
            "pull_request",
            "pull_request_target",
            "workflow_dispatch",
            "repository_dispatch",
            "schedule",
            "workflow_run",
            "unknown",
        ] {
            let old = WorkflowRun {
                workflow: 1,
                run: 10,
                event: event.into(),
                branch: "feature".into(),
            };
            let new = WorkflowRun {
                run: 20,
                ..old.clone()
            };
            let mut workflows = BTreeMap::from([(10, old), (20, new)]);
            let expected = if matches!(event, "push" | "pull_request" | "pull_request_target") {
                CheckState::Green
            } else {
                CheckState::Failed
            };
            assert_eq!(check_runs(&checks, &workflows), expected, "{event}");
            workflows.get_mut(&20).unwrap().run = 10;
            assert_eq!(
                check_runs(&checks, &workflows),
                CheckState::Green,
                "rerun: {event}"
            );
        }
    }

    #[test]
    fn only_verified_workflow_replacements_can_hide_a_failed_suite() {
        let checks = vec![
            json!({"id":300,"app":{"id":1,"slug":"github-actions"},"name":"test","status":"completed","conclusion":"failure","check_suite":{"id":10}}),
            json!({"id":200,"app":{"id":1,"slug":"github-actions"},"name":"test","status":"completed","conclusion":"success","check_suite":{"id":20}}),
        ];
        let old = WorkflowRun {
            workflow: 1,
            run: 10,
            event: "pull_request".into(),
            branch: "feature".into(),
        };
        let new = WorkflowRun {
            workflow: 1,
            run: 20,
            ..old.clone()
        };
        assert!(needs_workflow_identity(&checks));
        assert_eq!(check_runs(&checks, &BTreeMap::new()), CheckState::Failed);
        let mut workflows = BTreeMap::from([(10, old), (20, new)]);
        // The old workflow's failed job was created after the newer run's job.
        assert_eq!(check_runs(&checks, &workflows), CheckState::Green);
        workflows.get_mut(&20).unwrap().workflow = 2;
        assert_eq!(check_runs(&checks, &workflows), CheckState::Failed);
        workflows.get_mut(&20).unwrap().workflow = 1;
        workflows.get_mut(&20).unwrap().event = "push".into();
        assert_eq!(check_runs(&checks, &workflows), CheckState::Failed);
        workflows.get_mut(&20).unwrap().event = "pull_request".into();
        workflows.get_mut(&20).unwrap().branch = "other".into();
        assert_eq!(check_runs(&checks, &workflows), CheckState::Failed);
        workflows.remove(&10);
        assert_eq!(check_runs(&checks, &workflows), CheckState::Failed);
        let mut external = checks;
        for check in &mut external {
            check["app"]["slug"] = json!("other-ci");
        }
        assert!(!needs_workflow_identity(&external));
        assert_eq!(check_runs(&external, &workflows), CheckState::Failed);
    }

    #[test]
    fn superseded_runs_do_not_override_current_results() {
        let mut checks = vec![
            json!({"id":1,"app":{"id":1,"slug":"github-actions"},"name":"Redesign required","status":"completed","conclusion":"failure","check_suite":{"id":10}}),
            json!({"id":2,"app":{"id":1,"slug":"github-actions"},"check_suite":{"id":10},"name":"Repository format","status":"completed","conclusion":"cancelled"}),
            json!({"id":3,"app":{"id":1,"slug":"github-actions"},"name":"Redesign required","status":"completed","conclusion":"success","check_suite":{"id":20}}),
            json!({"id":4,"app":{"id":1,"slug":"github-actions"},"check_suite":{"id":20},"name":"Repository format","status":"completed","conclusion":"success"}),
            json!({"id":5,"app":{"id":1,"slug":"github-actions"},"name":"Admin test","status":"in_progress","conclusion":null}),
        ];
        let workflows = BTreeMap::from([
            (
                10,
                WorkflowRun {
                    workflow: 1,
                    run: 10,
                    event: "pull_request".into(),
                    branch: "feature".into(),
                },
            ),
            (
                20,
                WorkflowRun {
                    workflow: 1,
                    run: 20,
                    event: "pull_request".into(),
                    branch: "feature".into(),
                },
            ),
        ]);
        assert_eq!(check_runs(&checks, &workflows), CheckState::Running);
        checks.reverse();
        assert_eq!(check_runs(&checks, &workflows), CheckState::Running);
        checks[0]["status"] = json!("completed");
        checks[0]["conclusion"] = json!("success");
        assert_eq!(check_runs(&checks, &workflows), CheckState::Green);
        checks.push(json!({"id":6,"app":{"id":1,"slug":"github-actions"},"name":"Redesign required","status":"completed","conclusion":"failure"}));
        assert_eq!(check_runs(&checks, &workflows), CheckState::Failed);
    }

    #[test]
    fn independent_or_unidentified_failures_remain_blocking() {
        let successful = json!({"id":3,"app":{"id":1,"slug":"github-actions"},"name":"test","status":"completed","conclusion":"success"});
        for failed in [
            json!({"id":1,"app":{"id":2},"name":"test","status":"completed","conclusion":"failure"}),
            json!({"id":1,"app":{"id":1,"slug":"github-actions"},"name":"other test","status":"completed","conclusion":"cancelled"}),
            json!({"app":{"id":1,"slug":"github-actions"},"name":"test","status":"completed","conclusion":"failure"}),
        ] {
            assert_eq!(
                check_runs(&[failed, successful.clone()], &BTreeMap::new()),
                CheckState::Failed
            );
        }
        assert_eq!(check_runs(&[], &BTreeMap::new()), CheckState::Green);
    }

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
