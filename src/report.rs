//! "Report as incorrect" bundles: everything an agent needs to diagnose why
//! Gopher displayed a PR state the user considers wrong.
//!
//! Reports are local files under the data directory. They contain PR content
//! and GitHub responses but never credentials, and older reports are pruned.
use crate::{actions::Displayed, config::Config, model::PullRequest};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::BufRead,
    path::{Path, PathBuf},
};

/// Newest reports kept; older directories are removed after each write.
pub const MAX_REPORTS: usize = 50;
/// Recent log records for the PR included in a report.
const MAX_LOG_LINES: usize = 1000;
const SCHEMA: u32 = 1;

pub fn directory(data: &Path) -> PathBuf {
    data.join("reports")
}

pub struct Report<'a> {
    pub displayed: &'a Displayed,
    /// The worker's PR record before the confirming refetch.
    pub stored: &'a PullRequest,
    /// The state recomputed from the refetch, which matched the display.
    pub refetched: &'a PullRequest,
    pub evidence: &'a BTreeMap<String, Value>,
    pub config: &'a Config,
    pub viewer: Option<&'a str>,
    pub now: chrono::DateTime<chrono::Utc>,
}

/// Writes a complete report directory and returns its path. The bundle is
/// staged under a hidden name and renamed, so readers never see partial reports.
pub fn write(data: &Path, report: &Report<'_>) -> Result<PathBuf> {
    let reports = directory(data);
    std::fs::create_dir_all(&reports)?;
    let snapshot = &report.refetched.snapshot;
    let name = format!(
        "{}-{}-{}",
        report.now.format("%Y%m%dT%H%M%SZ"),
        sanitize(&snapshot.repo.replace('/', "-")),
        snapshot.number
    );
    let mut target = reports.join(&name);
    let mut suffix = 1;
    while target.exists() {
        suffix += 1;
        target = reports.join(format!("{name}-{suffix}"));
    }
    let staging = reports.join(format!(
        ".staging-{}",
        target.file_name().unwrap().to_string_lossy()
    ));
    let _ = std::fs::remove_dir_all(&staging);
    let written = (|| -> Result<()> {
        std::fs::create_dir_all(staging.join("github"))?;
        let files: Vec<_> = report
            .evidence
            .keys()
            .map(|name| format!("github/{}.json", sanitize(name)))
            .collect();
        write_json(&staging.join("report.json"), &summary(report, &files))?;
        for (name, value) in report.evidence {
            write_json(
                &staging
                    .join("github")
                    .join(format!("{}.json", sanitize(name))),
                value,
            )?;
        }
        let logs = pr_logs(
            &data.join("logs").join("gopher.jsonl"),
            &snapshot.repo,
            snapshot.number,
        );
        std::fs::write(staging.join("log.jsonl"), logs)?;
        std::fs::write(staging.join("README.md"), readme(report))?;
        Ok(())
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    std::fs::rename(&staging, &target)
        .with_context(|| format!("Cannot finish report {}", target.display()))?;
    prune(&reports, MAX_REPORTS);
    Ok(target)
}

fn summary(report: &Report<'_>, files: &[String]) -> Value {
    let snapshot = &report.refetched.snapshot;
    let expected = report
        .config
        .repositories
        .get(&snapshot.repo)
        .and_then(|repo| repo.reviewers.clone());
    json!({
        "schema": SCHEMA,
        "created_at": report.now.to_rfc3339(),
        "app": {
            "version": env!("CARGO_PKG_VERSION"),
            "revision": env!("GOPHER_BUILD_REVISION"),
            "build_status": env!("GOPHER_BUILD_STATUS"),
            "dev": crate::identity::IS_DEV,
        },
        "pr": {
            "id": snapshot.id,
            "repo": snapshot.repo,
            "number": snapshot.number,
            "url": snapshot.url,
            "title": snapshot.title,
        },
        "viewer": report.viewer,
        "user_claim": "The displayed review state and/or check summary is incorrect.",
        "displayed": report.displayed,
        "stored": report.stored,
        "refetched": report.refetched,
        "settings": {
            "expected_reviewers": expected,
            "poll_seconds": report.config.poll_seconds,
            "settle_seconds": report.config.settle_seconds,
            "review_start_grace_seconds": report.config.review_start_grace_seconds,
        },
        "files": {
            "github": files,
            "log": "log.jsonl",
        },
    })
}

fn readme(report: &Report<'_>) -> String {
    let pr = &report.refetched;
    let checks = pr
        .snapshot
        .check_state
        .map(|state| state.label_with(pr.snapshot.check_progress))
        .unwrap_or_else(|| "no check summary".into());
    let agents = pr
        .agents
        .iter()
        .map(|agent| {
            format!(
                "- {}: {:?} — {}",
                agent.agent.label(),
                agent.verdict,
                agent.reason
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"# Gopher report: {repo}#{number}

The user marked this PR's displayed state as **incorrect** at {time}.

- PR: {url}
- Head: `{head}`
- Displayed state: **{state}**, {checks}
- Reviewers:
{agents}

Gopher refetched the PR before writing this report and got the same result, so
the display was not merely stale: detection or aggregation produced a state
the user disagrees with. The user did not say what the correct state is;
compare the evidence with what GitHub shows on the PR page.

## Files

- `report.json`: displayed values (`displayed`), the stored PR record before
  the refetch (`stored`, including run history such as `head_since`,
  `candidate_id` and `reviewing_since`), the recomputed record (`refetched`,
  whose `snapshot` is the exact input to reviewer detection), and settings.
- `github/*.json`: raw GitHub responses, including what polling discards:
  every check run with its workflow trigger (`workflow_runs`), the timeline
  (force pushes, review requests), PR commits, untrimmed comments, reactions
  and reviews. Paginated lists are `{{"items": [...], "truncated": bool}}`;
  failed lookups are `{{"error": ...}}`.
- `log.jsonl`: recent Gopher log records for this PR (state transitions and
  reviewer evidence), without rate-limit noise.

## Diagnosing

1. Read `AGENTS.md` in the Gopher repository for the review-state rules.
2. Find which reviewer verdict or check input in `refetched` disagrees with
   the raw evidence, then trace it in `src/reviewers/` or
   `src/github/check_status.rs`.
3. `cargo run -- inspect {repo} {number}` reproduces live detection read-only.
4. Turn `refetched.snapshot` into a fixture-based regression test (see
   `tests/fixtures/README.md`) before changing detection.
"#,
        repo = pr.snapshot.repo,
        number = pr.snapshot.number,
        time = report.now.to_rfc3339(),
        url = pr.snapshot.url,
        head = pr.snapshot.head,
        state = pr.state.label(),
    )
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Log records for one PR, newest last, excluding per-request quota records.
fn pr_logs(path: &Path, repo: &str, number: u64) -> String {
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let matches = |value: &Value| value["repo"] == repo && value["pr"] == number;
    let mut lines = std::collections::VecDeque::new();
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if record["fields"]["event"] == "github_rate_limit" {
            continue;
        }
        if matches(&record["fields"]) || matches(&record["span"]) {
            if lines.len() == MAX_LOG_LINES {
                lines.pop_front();
            }
            lines.push_back(line);
        }
    }
    lines.into_iter().map(|line| line + "\n").collect()
}

fn prune(reports: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(reports) else {
        return;
    };
    let mut names: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    // Names start with a UTC timestamp, so lexical order is chronological.
    names.sort();
    let excess = names.len().saturating_sub(keep);
    for name in &names[..excess] {
        if let Err(error) = std::fs::remove_dir_all(reports.join(name)) {
            tracing::warn!(event = "report_prune_failed", report = %name, error = %error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model::Snapshot, worker::transition};

    fn pr() -> PullRequest {
        transition(
            Snapshot {
                id: "PR_1".into(),
                repo: "owner/repo".into(),
                number: 7,
                head: "head".into(),
                open: true,
                ..Default::default()
            },
            None,
            None,
            100,
            0,
        )
    }

    #[test]
    fn writes_complete_bundle_with_filtered_logs_and_prunes_old_reports() {
        let data = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(data.path().join("logs")).unwrap();
        std::fs::write(
            data.path().join("logs/gopher.jsonl"),
            [
                r#"{"fields":{"event":"review_state","repo":"owner/repo","pr":7}}"#,
                r#"{"fields":{"event":"github_rate_limit"},"span":{"repo":"owner/repo","pr":7}}"#,
                r#"{"fields":{"event":"review_state","repo":"owner/repo","pr":8}}"#,
                r#"{"fields":{"event":"snapshot"},"span":{"repo":"owner/repo","pr":7}}"#,
                "not json",
            ]
            .join("\n"),
        )
        .unwrap();
        for old in 0..MAX_REPORTS {
            std::fs::create_dir_all(directory(data.path()).join(format!("2000{old:04}"))).unwrap();
        }
        let pr = pr();
        let evidence = BTreeMap::from([
            (
                "timeline".to_owned(),
                json!({"items": [], "truncated": false}),
            ),
            ("check/runs".to_owned(), json!({"error": "unavailable"})),
        ]);
        let now = chrono::DateTime::from_timestamp(1_794_489_641, 0).unwrap();
        let path = write(
            data.path(),
            &Report {
                displayed: &Displayed::of(&pr),
                stored: &pr,
                refetched: &pr,
                evidence: &evidence,
                config: &Config::default(),
                viewer: Some("test"),
                now,
            },
        )
        .unwrap();
        assert_eq!(path.file_name().unwrap(), "20261112T132041Z-owner-repo-7");
        let summary: Value =
            serde_json::from_slice(&std::fs::read(path.join("report.json")).unwrap()).unwrap();
        assert_eq!(summary["pr"]["number"], 7);
        assert_eq!(summary["displayed"]["head"], "head");
        assert_eq!(summary["refetched"]["snapshot"]["repo"], "owner/repo");
        assert_eq!(
            summary["files"]["github"],
            json!(["github/check_runs.json", "github/timeline.json"])
        );
        assert!(path.join("github/check_runs.json").exists());
        assert!(path.join("README.md").exists());
        let logs = std::fs::read_to_string(path.join("log.jsonl")).unwrap();
        assert_eq!(logs.lines().count(), 2);
        assert!(!logs.contains("github_rate_limit") && !logs.contains("\"pr\":8"));
        let mut remaining: Vec<_> = std::fs::read_dir(directory(data.path()))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        remaining.sort();
        assert_eq!(remaining.len(), MAX_REPORTS);
        assert_eq!(remaining[0], "20000001");
        assert_eq!(remaining.last().unwrap(), "20261112T132041Z-owner-repo-7");
        // A second report in the same second gets its own directory.
        let second = write(
            data.path(),
            &Report {
                displayed: &Displayed::of(&pr),
                stored: &pr,
                refetched: &pr,
                evidence: &evidence,
                config: &Config::default(),
                viewer: None,
                now,
            },
        )
        .unwrap();
        assert_eq!(
            second.file_name().unwrap(),
            "20261112T132041Z-owner-repo-7-2"
        );
    }
}
