//! Raw, read-only GitHub evidence for "Report as incorrect" diagnostics.
//!
//! Polling keeps only the fields Gopher interprets. A report also needs what
//! it discarded (every check run and its trigger, the PR timeline, untrimmed
//! bodies) so a later investigation can see why detection went wrong.
use super::{Github, PrRef};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Bounds each paginated endpoint; reports are diagnostics, not archives.
const MAX_PAGES: usize = 10;

enum Shape {
    Object,
    Array,
    Field(&'static str),
}

const ROLLUP_QUERY: &str = "query($id:ID!){ node(id:$id){ ... on PullRequest { number state isDraft mergeable mergeStateStatus reviewDecision headRefOid baseRefName baseRefOid commits(last:1){ nodes{ commit{ oid statusCheckRollup{ state contexts(first:100){ pageInfo{ hasNextPage } nodes{ __typename ... on CheckRun{ name status conclusion startedAt completedAt checkSuite{ app{ slug } workflowRun{ event databaseId workflow{ name } } } } ... on StatusContext{ context state createdAt } } } } } } } } } }";

impl Github {
    /// Fetches each endpoint independently: a failure is recorded in place of
    /// that endpoint's data rather than discarding the rest of the evidence.
    pub async fn report_evidence(&self, reference: &PrRef, head: &str) -> BTreeMap<String, Value> {
        let (repo, number) = (&reference.repo, reference.number);
        let endpoints = [
            (
                "pull",
                format!("repos/{repo}/pulls/{number}"),
                Shape::Object,
            ),
            (
                "pull_commits",
                format!("repos/{repo}/pulls/{number}/commits"),
                Shape::Array,
            ),
            (
                "timeline",
                format!("repos/{repo}/issues/{number}/timeline"),
                Shape::Array,
            ),
            (
                "issue_comments",
                format!("repos/{repo}/issues/{number}/comments"),
                Shape::Array,
            ),
            (
                "reactions",
                format!("repos/{repo}/issues/{number}/reactions"),
                Shape::Array,
            ),
            (
                "reviews",
                format!("repos/{repo}/pulls/{number}/reviews"),
                Shape::Array,
            ),
            (
                "review_comments",
                format!("repos/{repo}/pulls/{number}/comments"),
                Shape::Array,
            ),
            (
                "check_runs",
                format!("repos/{repo}/commits/{head}/check-runs?filter=all"),
                Shape::Field("check_runs"),
            ),
            (
                "check_suites",
                format!("repos/{repo}/commits/{head}/check-suites"),
                Shape::Field("check_suites"),
            ),
            (
                "workflow_runs",
                format!("repos/{repo}/actions/runs?head_sha={head}"),
                Shape::Field("workflow_runs"),
            ),
            (
                "statuses",
                format!("repos/{repo}/commits/{head}/statuses"),
                Shape::Array,
            ),
        ];
        let mut evidence = BTreeMap::new();
        for (name, endpoint, shape) in endpoints {
            let value = self
                .report_endpoint(&endpoint, shape)
                .await
                .unwrap_or_else(|error| json!({"error": error.to_string(), "endpoint": endpoint}));
            evidence.insert(name.to_owned(), value);
        }
        let rollup = self
            .graphql(ROLLUP_QUERY, json!({"id": reference.id}))
            .await
            .unwrap_or_else(|error| json!({"error": error.to_string()}));
        evidence.insert("pull_graphql_rollup".into(), rollup);
        evidence
    }

    async fn report_endpoint(&self, endpoint: &str, shape: Shape) -> anyhow::Result<Value> {
        let rest = |endpoint: String| async move {
            self.execute(&["api", "--hostname", "github.com", &endpoint], None)
                .await
        };
        let field = match shape {
            Shape::Object => return rest(endpoint.to_owned()).await,
            Shape::Array => None,
            Shape::Field(field) => Some(field),
        };
        let separator = if endpoint.contains('?') { '&' } else { '?' };
        let mut items = Vec::new();
        let mut truncated = true;
        for page in 1..=MAX_PAGES {
            let data = rest(format!("{endpoint}{separator}per_page=100&page={page}")).await?;
            let values = match field {
                Some(field) => data[field].as_array(),
                None => data.as_array(),
            }
            .ok_or_else(|| anyhow::anyhow!("Unexpected response shape"))?;
            let count = values.len();
            items.extend(values.iter().cloned());
            if count < 100 {
                truncated = false;
                break;
            }
        }
        Ok(json!({"items": items, "truncated": truncated}))
    }
}
