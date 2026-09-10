mod coderabbit;
mod codex;
mod cubic;

use crate::model::*;
use std::collections::BTreeSet;

pub fn evaluate(
    snapshot: &Snapshot,
    previous: Option<&PullRequest>,
    expected: Option<&[Agent]>,
) -> Vec<AgentResult> {
    let observed: BTreeSet<Agent> = snapshot
        .reviews
        .iter()
        .filter_map(|r| Agent::from_login(&r.author))
        .chain(
            snapshot
                .comments
                .iter()
                .filter_map(|c| Agent::from_login(&c.author)),
        )
        .chain(
            snapshot
                .reactions
                .iter()
                .filter_map(|r| Agent::from_login(&r.author)),
        )
        .chain(
            snapshot
                .checks
                .iter()
                .filter_map(|c| Agent::from_app(&c.app)),
        )
        .collect();
    let agents: BTreeSet<Agent> = if let Some(expected) = expected {
        expected.iter().copied().collect()
    } else {
        observed
            .iter()
            .copied()
            .chain(
                previous
                    .into_iter()
                    // Explicit skips are excluded from inferred participation. A
                    // disappearing skip check must not create an unfinished review.
                    // Keep other observed reviewers until their result is known.
                    .flat_map(|p| p.agents.iter())
                    .filter(|a| a.verdict != Verdict::Skipped)
                    .map(|a| a.agent),
            )
            .collect()
    };
    agents
        .into_iter()
        .map(|agent| match agent {
            Agent::Codex => codex::detect(snapshot, previous),
            Agent::Cubic => cubic::detect(snapshot),
            Agent::CodeRabbit => coderabbit::detect(snapshot),
        })
        .map(|mut result| {
            if expected.is_none()
                && !observed.contains(&result.agent)
                && result.verdict == Verdict::Unknown
            {
                result.reason = format!(
                    "Previously participating reviewer has no current activity: {}",
                    result.reason
                );
            }
            if expected.is_some() && result.verdict == Verdict::Skipped {
                result.verdict = Verdict::Unknown;
                result.reason = format!("Required reviewer did not run: {}", result.reason);
            }
            result
        })
        .collect()
}

pub fn aggregate(snapshot: &Snapshot, agents: &[AgentResult]) -> State {
    let agents: Vec<_> = agents
        .iter()
        .filter(|a| a.verdict != Verdict::Skipped)
        .collect();
    if agents.iter().any(|a| a.verdict == Verdict::Running) {
        return State::Reviewing;
    }
    if agents.is_empty() || agents.iter().any(|a| a.verdict == Verdict::Unknown) {
        return State::Unknown;
    }
    if snapshot.threads.iter().any(|t| !t.resolved) {
        return State::Comments;
    }
    if agents.iter().all(|a| a.verdict == Verdict::Clean) {
        State::Approved
    } else {
        State::Unknown
    }
}

pub(super) fn result(
    agent: Agent,
    verdict: Verdict,
    run: impl Into<String>,
    reason: &str,
) -> AgentResult {
    AgentResult {
        agent,
        verdict,
        run_id: run.into(),
        reason: reason.into(),
    }
}
pub(super) fn current_review(s: &Snapshot, agent: Agent) -> Option<&Review> {
    s.reviews
        .iter()
        .filter(|r| {
            Agent::from_login(&r.author) == Some(agent)
                && r.commit == s.head
                && r.state != "PENDING"
        })
        .max_by_key(|r| (&r.submitted_at, &r.id))
}
pub(super) fn latest_checks(s: &Snapshot, agent: Agent) -> Vec<&Check> {
    let mut latest = std::collections::BTreeMap::new();
    for check in s
        .checks
        .iter()
        .filter(|c| Agent::from_app(&c.app) == Some(agent))
    {
        let entry = latest.entry(&check.name).or_insert(check);
        if (&check.started_at, check.id.parse::<u64>().unwrap_or(0))
            > (&entry.started_at, entry.id.parse::<u64>().unwrap_or(0))
        {
            *entry = check;
        }
    }
    latest.into_values().collect()
}
pub(super) fn check_gate(s: &Snapshot, agent: Agent) -> Option<AgentResult> {
    let checks = latest_checks(s, agent);
    if let Some(c) = checks.iter().find(|c| c.status != "completed") {
        return Some(result(
            agent,
            Verdict::Running,
            format!("check:{}:{}", c.id, c.started_at),
            "Agent check is queued or running",
        ));
    }
    if let Some(c) = checks
        .iter()
        .find(|c| !matches!(c.conclusion.as_str(), "success" | "neutral"))
    {
        return Some(result(
            agent,
            Verdict::Unknown,
            &c.id,
            "Agent check failed, was skipped, or was cancelled",
        ));
    }
    let branch_rewrite_skip =
        |c: &&Check| agent == Agent::Cubic && cubic::branch_rewrite_skip(&c.summary);
    if !checks.is_empty()
        && checks
            .iter()
            .all(|c| explicitly_skipped(&c.summary) || branch_rewrite_skip(c))
    {
        return Some(result(
            agent,
            Verdict::Skipped,
            checks
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>()
                .join(","),
            if checks.iter().any(branch_rewrite_skip) {
                "Review skipped — branch history rewritten; Cubic requires a manual review"
            } else {
                "Review skipped — subscription limit or reviewer paused"
            },
        ));
    }
    None
}
fn explicitly_skipped(summary: &str) -> bool {
    let summary = summary.to_lowercase();
    [
        "you've reached your plan's monthly review limit",
        "review skipped",
        "review is skipped",
        "reviews are paused",
    ]
    .iter()
    .any(|text| summary.contains(text))
}
pub(super) fn review_after_checks(s: &Snapshot, agent: Agent) -> Option<&Review> {
    let review = current_review(s, agent)?;
    // A previous clean review must not finish a newer rerun on the same SHA.
    if latest_checks(s, agent)
        .iter()
        .any(|c| !c.started_at.is_empty() && review.submitted_at < c.started_at)
    {
        return None;
    }
    Some(review)
}
pub(super) fn has_threads(s: &Snapshot, agent: Agent) -> bool {
    s.threads
        .iter()
        .any(|t| !t.resolved && Agent::from_login(&t.author) == Some(agent))
}
pub(super) fn matches_commit(text: &str, head: &str) -> bool {
    text.split(|c: char| !c.is_ascii_hexdigit())
        .any(|part| part.len() >= 7 && part.len() <= 40 && head.starts_with(part))
}
