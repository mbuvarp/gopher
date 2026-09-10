use super::*;
use regex::Regex;
use std::sync::LazyLock;
static COUNT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(\d+) issues? found\b").unwrap());

pub(super) fn branch_rewrite_skip(summary: &str) -> bool {
    summary.trim().to_lowercase().starts_with(
        "this push rewrote the branch history, so cubic did not start an automatic review.",
    )
}

fn verdict(text: &str) -> Option<Verdict> {
    // Restrict review-body interpretation to Cubic's summary, excluding quoted discussion.
    let text = text
        .split("cubic:review-summary:start -->")
        .nth(1)
        .unwrap_or(text)
        .split("<!-- cubic:review-summary:end")
        .next()
        .unwrap_or(text);
    let lower = text.to_lowercase();
    if lower.contains("no issues found") || lower.contains("all reported issues were addressed") {
        return Some(Verdict::Clean);
    }
    COUNT.captures(text).map(|c| {
        if &c[1] == "0" {
            Verdict::Clean
        } else {
            Verdict::Findings
        }
    })
}

pub(super) fn detect(s: &Snapshot) -> AgentResult {
    let agent = Agent::Cubic;
    if let Some(gate) = check_gate(s, agent) {
        return gate;
    }
    if let Some(r) = review_after_checks(s, agent)
        && let Some(v) =
            verdict(&r.body).or_else(|| (r.state == "APPROVED").then_some(Verdict::Clean))
    {
        return result(agent, v, &r.id, "Current-commit Cubic review summary");
    }
    let checks = latest_checks(s, agent);
    if !checks.is_empty() {
        let values: Vec<_> = checks.iter().filter_map(|c| verdict(&c.summary)).collect();
        if values.len() == checks.len() {
            let v = if values.contains(&Verdict::Findings) {
                Verdict::Findings
            } else {
                Verdict::Clean
            };
            return result(
                agent,
                v,
                checks
                    .iter()
                    .map(|c| c.id.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
                "Current-commit Cubic check summary",
            );
        }
    }
    result(
        agent,
        Verdict::Unknown,
        "",
        "No conclusive Cubic result for the current commit",
    )
}
