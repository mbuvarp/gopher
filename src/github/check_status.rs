//! Summarize the CI checks GitHub shows on a PR, from its status rollup.
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

/// Aggregate state and progress over the latest result for each counted check.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Summary {
    pub(super) state: CheckState,
    pub(super) progress: CheckProgress,
}

impl Summary {
    fn record(&mut self, state: CheckState) {
        self.state = self.state.max(state);
        self.progress.record(state);
    }

    pub(super) fn merge(self, other: Self) -> Self {
        Self {
            state: self.state.max(other.state),
            progress: CheckProgress {
                completed: self.progress.completed + other.progress.completed,
                total: self.progress.total + other.progress.total,
            },
        }
    }
}

impl FromIterator<CheckState> for Summary {
    fn from_iter<I: IntoIterator<Item = CheckState>>(states: I) -> Self {
        let mut summary = Self::default();
        states.into_iter().for_each(|state| summary.record(state));
        summary
    }
}

struct RankedCheck<'a> {
    order: (u64, u64),
    check: &'a Value,
}

/// Split GitHub's PR status rollup into REST-shaped check runs, legacy statuses,
/// and the workflow identity of each Actions check suite. The rollup is what
/// GitHub shows on the PR: it already leaves out runs that are not part of the
/// PR's checks, such as manually dispatched workflows.
pub(super) fn rollup(contexts: &[Value]) -> (Vec<Value>, Vec<Value>, BTreeMap<u64, WorkflowRun>) {
    let lower = |value: &Value| value.as_str().map(str::to_lowercase);
    let mut checks = Vec::new();
    let mut statuses = Vec::new();
    let mut workflows = BTreeMap::new();
    for context in contexts {
        match context["__typename"].as_str() {
            Some("CheckRun") => {
                let suite = &context["checkSuite"];
                let run = &suite["workflowRun"];
                if let (Some(suite_id), Some(workflow), Some(id), Some(event)) = (
                    suite["databaseId"].as_u64(),
                    run["workflow"]["databaseId"].as_u64(),
                    run["databaseId"].as_u64(),
                    run["event"].as_str(),
                ) {
                    workflows.insert(
                        suite_id,
                        WorkflowRun {
                            workflow,
                            run: id,
                            event: event.into(),
                            branch: suite["branch"]["name"].as_str().unwrap_or("").into(),
                        },
                    );
                }
                checks.push(json!({
                    "id": context["databaseId"],
                    "app": {"id": suite["app"]["databaseId"], "slug": suite["app"]["slug"]},
                    "name": context["name"],
                    "status": lower(&context["status"]),
                    "conclusion": lower(&context["conclusion"]),
                    "check_suite": {"id": suite["databaseId"]},
                }));
            }
            Some("StatusContext") => {
                // A required context that has not reported yet is still pending.
                let state = match lower(&context["state"]).as_deref() {
                    Some("expected") => Some("pending".to_owned()),
                    state => state.map(str::to_owned),
                };
                statuses.push(json!({
                    "id": statuses.len(),
                    "context": context["context"],
                    "state": state,
                }));
            }
            // The rollup context union only has check runs and statuses.
            _ => {}
        }
    }
    (checks, statuses, workflows)
}

pub(super) fn check_runs(checks: &[Value], workflows: &BTreeMap<u64, WorkflowRun>) -> Summary {
    // Names alone do not establish replacement: independent Actions workflows
    // often both use `test`. Only verified workflow identity can join suites.
    let mut latest: BTreeMap<(String, Scope, &str), RankedCheck<'_>> = BTreeMap::new();
    let mut summary = Summary::default();
    for check in checks {
        // Replacement semantics are specific to GitHub Actions. Other apps
        // can publish independent checks with identical names in one suite.
        if check["app"]["slug"] != "github-actions" {
            summary.record(check_run(check));
            continue;
        }
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
            summary.record(check_run(check));
            continue;
        };
        let workflow = workflows.get(&suite);
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
    for ranked in latest.values() {
        summary.record(check_run(ranked.check));
    }
    summary
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

pub(super) fn commit_statuses(statuses: &[Value]) -> Summary {
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
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_actions_checks_with_the_same_name_and_suite_remain_independent() {
        for app in [
            json!({"id":2,"slug":"other-ci"}),
            json!({"id":2}),
            Value::Null,
        ] {
            let mut checks = vec![
                json!({"id":100,"app":app,"name":"test","status":"completed","conclusion":"failure","check_suite":{"id":10}}),
                json!({"id":200,"app":app,"name":"test","status":"completed","conclusion":"success","check_suite":{"id":10}}),
            ];
            assert_eq!(
                check_runs(&checks, &BTreeMap::new()).state,
                CheckState::Failed
            );
            checks.reverse();
            assert_eq!(
                check_runs(&checks, &BTreeMap::new()).state,
                CheckState::Failed
            );
            checks[1]["status"] = json!("in_progress");
            checks[1]["conclusion"] = Value::Null;
            assert_eq!(
                check_runs(&checks, &BTreeMap::new()).state,
                CheckState::Running
            );
            assert_eq!(
                check_runs(&checks, &BTreeMap::new()).progress,
                CheckProgress {
                    completed: 1,
                    total: 2
                }
            );
        }
    }

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
            assert_eq!(check_runs(&checks, &workflows).state, expected, "{event}");
            workflows.get_mut(&20).unwrap().run = 10;
            assert_eq!(
                check_runs(&checks, &workflows).state,
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
        assert_eq!(
            check_runs(&checks, &BTreeMap::new()).state,
            CheckState::Failed
        );
        let mut workflows = BTreeMap::from([(10, old), (20, new)]);
        // The old workflow's failed job was created after the newer run's job.
        assert_eq!(check_runs(&checks, &workflows).state, CheckState::Green);
        workflows.get_mut(&20).unwrap().workflow = 2;
        assert_eq!(check_runs(&checks, &workflows).state, CheckState::Failed);
        workflows.get_mut(&20).unwrap().workflow = 1;
        workflows.get_mut(&20).unwrap().event = "push".into();
        assert_eq!(check_runs(&checks, &workflows).state, CheckState::Failed);
        workflows.get_mut(&20).unwrap().event = "pull_request".into();
        workflows.get_mut(&20).unwrap().branch = "other".into();
        assert_eq!(check_runs(&checks, &workflows).state, CheckState::Failed);
        workflows.remove(&10);
        assert_eq!(check_runs(&checks, &workflows).state, CheckState::Failed);
        let mut external = checks;
        for check in &mut external {
            check["app"]["slug"] = json!("other-ci");
        }
        assert_eq!(check_runs(&external, &workflows).state, CheckState::Failed);
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
        assert_eq!(check_runs(&checks, &workflows).state, CheckState::Running);
        // Superseded jobs are not counted towards progress either.
        assert_eq!(
            check_runs(&checks, &workflows).progress,
            CheckProgress {
                completed: 2,
                total: 3
            }
        );
        checks.reverse();
        assert_eq!(check_runs(&checks, &workflows).state, CheckState::Running);
        checks[0]["status"] = json!("completed");
        checks[0]["conclusion"] = json!("success");
        assert_eq!(check_runs(&checks, &workflows).state, CheckState::Green);
        checks.push(json!({"id":6,"app":{"id":1,"slug":"github-actions"},"name":"Redesign required","status":"completed","conclusion":"failure"}));
        assert_eq!(check_runs(&checks, &workflows).state, CheckState::Failed);
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
                check_runs(&[failed, successful.clone()], &BTreeMap::new()).state,
                CheckState::Failed
            );
        }
        assert_eq!(check_runs(&[], &BTreeMap::new()).state, CheckState::Green);
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
        assert_eq!(commit_statuses(&statuses).state, CheckState::Green);
        statuses.reverse();
        assert_eq!(commit_statuses(&statuses).state, CheckState::Green);
        assert_eq!(
            commit_statuses(&statuses).progress,
            CheckProgress {
                completed: 2,
                total: 2
            }
        );
        statuses.push(json!({"id":5,"context":"tests","state":"pending"}));
        assert_eq!(commit_statuses(&statuses).state, CheckState::Running);
        assert_eq!(
            commit_statuses(&statuses).progress,
            CheckProgress {
                completed: 1,
                total: 2
            }
        );
        statuses.push(json!({"id":6,"context":"deploy","state":"error"}));
        assert_eq!(commit_statuses(&statuses).state, CheckState::Failed);
        assert_eq!(commit_statuses(&[]).state, CheckState::Green);
        assert_eq!(commit_statuses(&[]).progress, CheckProgress::default());
    }

    #[test]
    fn summaries_merge_state_and_progress() {
        let runs: Summary = [CheckState::Green, CheckState::Running]
            .into_iter()
            .collect();
        let statuses: Summary = [CheckState::Green, CheckState::Green, CheckState::Green]
            .into_iter()
            .collect();
        let merged = runs.merge(statuses);
        assert_eq!(merged.state, CheckState::Running);
        assert_eq!(
            merged.progress,
            CheckProgress {
                completed: 4,
                total: 5
            }
        );
        let failed: Summary = [CheckState::Failed].into_iter().collect();
        assert_eq!(merged.merge(failed).state, CheckState::Failed);
        assert_eq!(merged.merge(failed).progress.completed, 5);
    }

    fn rollup_run(
        id: u64,
        suite: u64,
        run: u64,
        event: &str,
        status: &str,
        conclusion: Value,
    ) -> Value {
        json!({
            "__typename":"CheckRun","databaseId":id,"name":"test","status":status,"conclusion":conclusion,
            "checkSuite":{"databaseId":suite,"branch":{"name":"feature"},"app":{"databaseId":15368,"slug":"github-actions"},
                "workflowRun":{"databaseId":run,"event":event,"workflow":{"databaseId":1}}}
        })
    }

    #[test]
    fn rollup_contexts_keep_replacement_rules_and_legacy_states() {
        // An older automatic run's failure is replaced by its newer rerun.
        let contexts = vec![
            rollup_run(1, 10, 10, "pull_request", "COMPLETED", json!("FAILURE")),
            rollup_run(2, 20, 20, "pull_request", "IN_PROGRESS", Value::Null),
            json!({"__typename":"StatusContext","context":"deploy","state":"SUCCESS"}),
            json!({"__typename":"StatusContext","context":"required","state":"EXPECTED"}),
        ];
        let (checks, statuses, workflows) = rollup(&contexts);
        assert_eq!(workflows.len(), 2);
        let runs = check_runs(&checks, &workflows);
        assert_eq!(runs.state, CheckState::Running);
        assert_eq!(
            runs.progress,
            CheckProgress {
                completed: 0,
                total: 1
            }
        );
        let legacy = commit_statuses(&statuses);
        assert_eq!(legacy.state, CheckState::Running);
        assert_eq!(
            legacy.progress,
            CheckProgress {
                completed: 1,
                total: 2
            }
        );
        // Separately triggered executions stay independent of each other.
        let manual = vec![
            rollup_run(1, 10, 10, "workflow_run", "COMPLETED", json!("FAILURE")),
            rollup_run(2, 20, 20, "workflow_run", "COMPLETED", json!("SUCCESS")),
        ];
        let (checks, _, workflows) = rollup(&manual);
        assert_eq!(check_runs(&checks, &workflows).state, CheckState::Failed);
    }

    #[test]
    fn rollup_summary_matches_the_checks_github_shows_on_the_pr() {
        // Ur-Solutions/ngs#108: the head also had iOS and Android runs started by
        // workflow_dispatch, still running long after the PR check passed.
        // GitHub's rollup lists only the pull_request run, so the PR is green.
        let contexts = vec![json!({
            "__typename":"CheckRun","databaseId":131197842241u64,"name":"Lint, typecheck and test",
            "status":"COMPLETED","conclusion":"SUCCESS",
            "checkSuite":{"databaseId":117042807844u64,"branch":{"name":"feature/ngs-114"},
                "app":{"databaseId":15368,"slug":"github-actions"},
                "workflowRun":{"databaseId":43222660560u64,"event":"pull_request","workflow":{"databaseId":405945880}}}
        })];
        let (checks, statuses, workflows) = rollup(&contexts);
        let summary = check_runs(&checks, &workflows).merge(commit_statuses(&statuses));
        assert_eq!(summary.state, CheckState::Green);
        assert_eq!(
            summary.progress,
            CheckProgress {
                completed: 1,
                total: 1
            }
        );
        let (checks, statuses, workflows) = rollup(&[]);
        assert!(checks.is_empty() && statuses.is_empty() && workflows.is_empty());
    }
}
