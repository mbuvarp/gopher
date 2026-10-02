use super::*;

/// Codex comments instead of reviewing when its usage limit is reached. The
/// notice stays on the PR, so it only counts while it is strictly newer than all
/// other Codex activity, including a leftover eyes reaction. Ties are ambiguous.
fn usage_limit<'a>(s: &'a Snapshot, reactions: &[&Reaction]) -> Option<&'a Comment> {
    let is_notice = |c: &Comment| {
        c.body
            .to_lowercase()
            .contains("reached your codex usage limit")
    };
    let codex_comments = || {
        s.comments
            .iter()
            .filter(|c| Agent::from_login(&c.author) == Some(Agent::Codex))
    };
    let notice = codex_comments()
        .filter(|c| is_notice(c))
        .max_by_key(|c| &c.updated_at)?;
    let since = notice.updated_at.as_str();
    let newer_comment = codex_comments().any(|c| !is_notice(c) && c.updated_at.as_str() >= since);
    let newer_review = s.reviews.iter().any(|r| {
        Agent::from_login(&r.author) == Some(Agent::Codex) && r.submitted_at.as_str() >= since
    });
    let newer_reaction = reactions.iter().any(|r| r.created_at.as_str() >= since);
    (!newer_comment && !newer_review && !newer_reaction).then_some(notice)
}

pub(super) fn detect(s: &Snapshot, previous: Option<&PullRequest>) -> AgentResult {
    let agent = Agent::Codex;
    let reactions: Vec<_> = s
        .reactions
        .iter()
        .filter(|r| Agent::from_login(&r.author) == Some(agent))
        .collect();
    if let Some(notice) = usage_limit(s, &reactions) {
        return result(
            agent,
            Verdict::Skipped,
            &notice.id,
            "Review skipped — Codex usage limit reached",
        );
    }
    if let Some(eyes) = reactions.iter().find(|r| r.content == "EYES") {
        return result(
            agent,
            Verdict::Running,
            &eyes.id,
            "Codex eyes reaction is present",
        );
    }
    let summary = s
        .comments
        .iter()
        .filter(|c| {
            Agent::from_login(&c.author) == Some(agent)
                && c.body
                    .contains("<!-- codex-pull-request-review-summary -->")
        })
        .max_by_key(|c| &c.updated_at);
    if let Some(summary) = summary {
        let rows: Vec<_> = summary
            .body
            .lines()
            .filter(|line| line.starts_with('|') && line.contains('`'))
            .collect();
        let current_rows: Vec<_> = rows
            .iter()
            .copied()
            .filter(|row| matches_commit(row.split('|').nth(3).unwrap_or(""), &s.head))
            .collect();
        if current_rows.is_empty() {
            // A current-commit review shows Codex already started, so the
            // stale summary is missing evidence rather than awaiting a review.
            if current_review(s, agent).is_some() {
                return result(
                    agent,
                    Verdict::Unknown,
                    &summary.id,
                    "Codex summary is missing current-commit evidence",
                );
            }
            return result(
                agent,
                Verdict::Pending,
                &summary.id,
                "Codex summary has no review for the current commit yet",
            );
        }
        if current_rows.iter().any(|row| {
            row.split('|').nth(2).is_some_and(|cell| {
                cell.to_lowercase().contains("running") || cell.to_lowercase().contains("queued")
            })
        }) {
            return result(
                agent,
                Verdict::Running,
                format!("{}:{}", summary.id, summary.updated_at),
                "Codex summary contains a running review",
            );
        }
        let statuses: Vec<_> = current_rows
            .iter()
            .map(|row| row.split('|').nth(2).unwrap_or(""))
            .collect();
        let failed: Vec<_> = current_rows
            .iter()
            .zip(&statuses)
            .filter(|(_, status)| {
                status
                    .split(|c: char| !c.is_ascii_alphabetic())
                    .find(|word| !word.is_empty())
                    .is_some_and(|word| word.eq_ignore_ascii_case("failed"))
            })
            .map(|(row, _)| *row)
            .collect();
        if !failed.is_empty() {
            return result(
                agent,
                Verdict::Failed,
                format!("{}:{}", summary.id, crate::model::hash(failed.join("\n"))),
                "Codex summary reports a failed review for the current commit",
            );
        }
        if current_rows.len() != rows.len() {
            return result(
                agent,
                Verdict::Unknown,
                &summary.id,
                "Codex summary includes review activity for an older commit",
            );
        }
        let statuses: Vec<_> = statuses
            .iter()
            .map(|status| status.to_lowercase())
            .collect();
        if statuses.iter().any(|status| {
            ["cancelled", "canceled", "skipped", "error"]
                .iter()
                .any(|word| status.contains(word))
        }) {
            return result(
                agent,
                Verdict::Unknown,
                &summary.id,
                "Codex review did not complete successfully",
            );
        }
        let complete = statuses.iter().all(|status| {
            [
                "completed",
                "complete",
                "finished",
                "no findings",
                "no issues",
                "findings found",
            ]
            .iter()
            .any(|word| status.contains(word))
        });
        if complete {
            let findings = has_threads(s, agent)
                || statuses.iter().any(|status| {
                    status.contains("findings")
                        && !status.contains("no findings")
                        && !status.contains("0 findings")
                });
            let clean = statuses.iter().all(|status| {
                status.contains("no findings")
                    || status.contains("0 findings")
                    || status.contains("no issues")
            });
            let thumb = reactions
                .iter()
                .any(|r| r.content == "THUMBS_UP" && r.created_at >= summary.updated_at);
            let v = if findings {
                Verdict::Findings
            } else if clean || thumb {
                Verdict::Clean
            } else {
                Verdict::Unknown
            };
            return result(
                agent,
                v,
                format!("{}:{}", summary.id, summary.updated_at),
                "Current-commit Codex summary; all listed runs finished",
            );
        }
        return result(
            agent,
            Verdict::Unknown,
            &summary.id,
            "Unrecognized Codex summary status",
        );
    }
    // Without a summary, a newly observed reaction is safe only after observing activity on this SHA.
    if let Some(prev) = previous.filter(|p| p.snapshot.head == s.head) {
        let old = prev.agents.iter().find(|a| a.agent == agent);
        let awaiting =
            old.is_some_and(|a| a.verdict == Verdict::Running || a.run_id.starts_with("awaiting:"));
        let thumb = reactions.iter().find(|r| r.content == "THUMBS_UP");
        if let (Some(old), Some(thumb)) = (old, thumb) {
            let new_thumb = !prev.snapshot.reactions.iter().any(|r| r.id == thumb.id);
            if (awaiting && new_thumb && !prev.stale)
                || (old.verdict == Verdict::Clean && old.run_id == thumb.id)
            {
                return result(
                    agent,
                    Verdict::Clean,
                    &thumb.id,
                    "Codex approval observed after review on this commit",
                );
            }
        }
        // An old submitted review cannot complete a newly observed rerun.
        if awaiting {
            let new_review = current_review(s, agent)
                .filter(|r| !prev.snapshot.reviews.iter().any(|old| old.id == r.id));
            if let Some(r) = new_review {
                return result(
                    agent,
                    Verdict::Findings,
                    &r.id,
                    "Codex posted a new review after observed activity",
                );
            }
            return result(
                agent,
                Verdict::Unknown,
                old.map(|a| {
                    if a.run_id.starts_with("awaiting:") {
                        a.run_id.clone()
                    } else {
                        format!("awaiting:{}", a.run_id)
                    }
                })
                .unwrap_or_default(),
                "Codex activity ended without a new result",
            );
        }
    }
    if let Some(r) = current_review(s, agent) {
        let v = if r.state == "APPROVED" {
            Verdict::Clean
        } else if matches!(r.state.as_str(), "COMMENTED" | "CHANGES_REQUESTED") {
            Verdict::Findings
        } else {
            Verdict::Unknown
        };
        return result(
            agent,
            v,
            &r.id,
            "Codex submitted a review for the current commit",
        );
    }
    result(
        agent,
        Verdict::Pending,
        "",
        "No Codex activity is tied to the current commit yet",
    )
}
