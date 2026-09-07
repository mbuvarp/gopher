use super::*;

pub(super) fn detect(s: &Snapshot, previous: Option<&PullRequest>) -> AgentResult {
    let agent = Agent::Codex;
    let reactions: Vec<_> = s
        .reactions
        .iter()
        .filter(|r| Agent::from_login(&r.author) == Some(agent))
        .collect();
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
        if rows.iter().any(|row| {
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
        if rows.is_empty()
            || rows
                .iter()
                .any(|row| !matches_commit(row.split('|').nth(3).unwrap_or(""), &s.head))
        {
            return result(
                agent,
                Verdict::Unknown,
                &summary.id,
                "Codex summary is missing current-commit evidence",
            );
        }
        let statuses: Vec<_> = rows
            .iter()
            .map(|row| row.split('|').nth(2).unwrap_or("").to_lowercase())
            .collect();
        if statuses.iter().any(|status| {
            ["failed", "cancelled", "canceled", "skipped", "error"]
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
        Verdict::Unknown,
        "",
        "Codex reaction cannot be tied to the current commit",
    )
}
