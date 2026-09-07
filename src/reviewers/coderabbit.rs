use super::*;
use regex::Regex;
use std::sync::LazyLock;
static COUNT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)actionable comments posted:\s*\**\s*(\d+)\b").unwrap());

pub(super) fn detect(s: &Snapshot) -> AgentResult {
    let agent = Agent::CodeRabbit;
    if let Some(gate) = check_gate(s, agent) {
        return gate;
    }
    if let Some(r) = review_after_checks(s, agent) {
        let body = r.body.to_lowercase();
        let v = if r.state == "APPROVED" || body.contains("no actionable comments were generated") {
            Some(Verdict::Clean)
        } else if let Some(c) = COUNT.captures(&r.body) {
            Some(if &c[1] == "0" {
                Verdict::Clean
            } else {
                Verdict::Findings
            })
        } else if r.state == "CHANGES_REQUESTED" {
            Some(Verdict::Findings)
        } else {
            None
        };
        if let Some(v) = v {
            return result(agent, v, &r.id, "Current-commit CodeRabbit review verdict");
        }
    }
    // A successful check can mean a skipped/limited review; do not equate success with approval.
    result(
        agent,
        Verdict::Unknown,
        "",
        "No explicit CodeRabbit verdict for the current commit",
    )
}
