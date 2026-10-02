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
    let observed = observed_agents(snapshot);
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
            if result.verdict == Verdict::Pending
                && !observed_new_head(snapshot, previous, result.agent)
            {
                result.verdict = Verdict::Unknown;
            }
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
                result.reason = format!("{REQUIRED_SKIP}: {}", result.reason);
            }
            result
        })
        .collect()
}

/// Waiting for a reviewer to start is only safe after Gopher observed the head
/// change itself, for a reviewer that participated on the earlier head. Without
/// that (such as on a cold start), the reviewer may have been stalled on this
/// commit for a long time, so the evidence stays unknown.
fn observed_new_head(snapshot: &Snapshot, previous: Option<&PullRequest>, agent: Agent) -> bool {
    let Some(pr) = previous.filter(|pr| pr.fetched_at > 0) else {
        return false;
    };
    pr.agents.iter().filter(|a| a.agent == agent).any(|a| {
        let waiting = a.verdict == Verdict::Pending
            || (a.verdict == Verdict::Unknown && a.reason.contains(NOT_STARTED));
        // Explicit skips are not participation, including required reviewers
        // whose skip was reported as unknown.
        let skipped = a.verdict == Verdict::Skipped || a.reason.starts_with(REQUIRED_SKIP);
        let participated = !skipped
            && (a.verdict != Verdict::Unknown
                || !a.run_id.is_empty()
                || observed_agents(&pr.snapshot).contains(&agent));
        waiting || (pr.snapshot.head != snapshot.head && participated)
    })
}

const REQUIRED_SKIP: &str = "Required reviewer did not run";
const NOT_STARTED: &str = "has not started reviewing the new commit within";

/// Once the grace period after a new head has passed, reviewers that still have
/// not started are unknown. Keeping the reason marks the fallback as settled.
pub fn expire_pending(agents: &mut [AgentResult], grace_seconds: u64) {
    for agent in agents.iter_mut().filter(|a| a.verdict == Verdict::Pending) {
        agent.verdict = Verdict::Unknown;
        agent.reason = format!(
            "{} {NOT_STARTED} {}",
            agent.agent.label(),
            duration_label(grace_seconds)
        );
    }
}

fn duration_label(seconds: u64) -> String {
    match (seconds / 60, seconds % 60) {
        (0, seconds) => format!("{seconds}s"),
        (minutes, 0) => format!("{minutes}m"),
        (minutes, seconds) => format!("{minutes}m {seconds}s"),
    }
}

fn observed_agents(snapshot: &Snapshot) -> BTreeSet<Agent> {
    snapshot
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
        .collect()
}

/// A fresh, non-draft PR can wait for its first review without implying a
/// detection failure. Explicit skips (such as reviews skipped for drafts or
/// paused subscriptions) are not participation. Once a reviewer has
/// participated, missing evidence stays unknown until a current result is
/// observed.
pub fn ready_for_review(
    snapshot: &Snapshot,
    previous: Option<&PullRequest>,
    agents: &[AgentResult],
    expected: Option<&[Agent]>,
) -> bool {
    (expected.is_none() || !agents.is_empty())
        && snapshot.open
        && !snapshot.draft
        && !snapshot.threads.iter().any(|thread| !thread.resolved)
        // Required reviewers never report Skipped, so their activity always blocks.
        && observed_agents(snapshot).iter().all(|agent| {
            agents
                .iter()
                .find(|result| result.agent == *agent)
                .map_or(expected.is_some(), |result| {
                    result.verdict == Verdict::Skipped
                })
        })
        && agents
            .iter()
            .all(|agent| matches!(agent.verdict, Verdict::Unknown | Verdict::Skipped))
        && !agents.iter().any(|agent| {
            agent
                .reason
                .starts_with("Previously participating reviewer has no current activity")
                // Reviewers that never started on a new head participated earlier.
                || agent.reason.contains(NOT_STARTED)
        })
        && !previous.is_some_and(|pr| {
            let observed = observed_agents(&pr.snapshot);
            pr.agents.iter().any(|agent| {
                (expected.is_none() || agents.iter().any(|result| result.agent == agent.agent))
                    && agent.verdict != Verdict::Skipped
                    && (agent.verdict != Verdict::Unknown
                        || !agent.run_id.is_empty()
                        || observed.contains(&agent.agent))
            })
        })
}

pub fn aggregate(snapshot: &Snapshot, agents: &[AgentResult]) -> State {
    let agents: Vec<_> = agents
        .iter()
        .filter(|a| a.verdict != Verdict::Skipped)
        .collect();
    if agents.iter().any(|a| a.verdict == Verdict::Running) {
        return State::Reviewing;
    }
    if agents.iter().any(|a| a.verdict == Verdict::Failed) {
        return State::Failed;
    }
    if agents.is_empty() || agents.iter().any(|a| a.verdict == Verdict::Unknown) {
        return State::Unknown;
    }
    // Results from reviewers that already finished cannot be final while
    // another reviewer has yet to start on the current commit. Drafts are
    // never ready for review.
    if agents.iter().any(|a| a.verdict == Verdict::Pending) {
        return if snapshot.open && !snapshot.draft {
            State::ReadyForReview
        } else {
            State::Unknown
        };
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
    // A current-commit review posted after the skip (such as a manual run)
    // is participation, so let the detector evaluate it instead.
    if !checks.is_empty()
        && checks
            .iter()
            .all(|c| explicitly_skipped(&c.summary) || branch_rewrite_skip(c))
        && review_after_checks(s, agent).is_none()
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
                skip_reason(&checks)
            },
        ));
    }
    None
}
fn skip_reason(checks: &[&Check]) -> &'static str {
    let summaries: Vec<_> = checks.iter().map(|c| c.summary.to_lowercase()).collect();
    let any = |text: &str| summaries.iter().any(|summary| summary.contains(text));
    if any("review limit") {
        "Review skipped — subscription limit reached"
    } else if any("paused") {
        "Review skipped — reviews are paused"
    } else if any("automatic reviews are disabled") {
        "Review skipped — automatic reviews are disabled"
    } else {
        "Review skipped by the reviewer"
    }
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
