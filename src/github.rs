use crate::{config::Config, model::*};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::{io::AsyncWriteExt, process::Command};

#[derive(Clone)]
pub struct Github {
    executable: PathBuf,
    timeout: Duration,
}
#[derive(Clone, Debug)]
pub struct PrRef {
    pub id: String,
    pub repo: String,
    pub number: u64,
}

pub fn resolve_gh(config: &Config) -> Result<PathBuf> {
    if let Some(path) = &config.gh_path {
        ensure!(
            path.is_absolute() && path.is_file(),
            "Configured gh_path must be an existing absolute executable path"
        );
        return Ok(path.clone());
    }
    let search_path = std::env::var_os("PATH").unwrap_or_default();
    let candidates = std::env::split_paths(&search_path)
        .map(|path| path.join("gh"))
        .chain(["/opt/homebrew/bin/gh", "/usr/local/bin/gh", "/usr/bin/gh"].map(PathBuf::from));
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .context("GitHub CLI is missing. Install gh, then run gh auth login.")
}

impl Github {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            executable: resolve_gh(config)?,
            timeout: Duration::from_secs(config.request_timeout_seconds),
        })
    }

    async fn execute(&self, args: &[&str], payload: Option<&Value>) -> Result<Value> {
        let start = Instant::now();
        let request = request_kind(args, payload);
        let mut command = Command::new(&self.executable);
        command
            .args(args)
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_PAGER", "cat")
            .env("GH_HOST", "github.com")
            .env_remove("GH_DEBUG")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .context("Cannot start GitHub CLI; check gh_path and executable permissions")?;
        let operation = async {
            if let Some(payload) = payload {
                let mut input = child.stdin.take().context("gh stdin unavailable")?;
                input.write_all(&serde_json::to_vec(payload)?).await?;
            } else {
                drop(child.stdin.take());
            }
            let output = child.wait_with_output().await?;
            tracing::debug!(
                event = "github_request",
                elapsed_ms = start.elapsed().as_millis() as u64,
                success = output.status.success()
            );
            if !output.status.success() {
                let failure = classify_failure(&String::from_utf8_lossy(&output.stderr));
                tracing::warn!(
                    event = "github_request_failed",
                    request,
                    category = failure.category,
                    http_status = failure.http_status,
                    exit_code = output.status.code(),
                    elapsed_ms = start.elapsed().as_millis() as u64
                );
                bail!("{}", failure.message);
            }
            ensure!(
                output.stdout.len() <= 32 * 1024 * 1024,
                "GitHub response exceeded 32 MiB"
            );
            serde_json::from_slice(&output.stdout).context("GitHub CLI returned invalid JSON")
        };
        match tokio::time::timeout(self.timeout, operation).await {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!(
                    event = "github_request_failed",
                    request,
                    category = "timeout",
                    elapsed_ms = start.elapsed().as_millis() as u64
                );
                bail!("GitHub request timed out; check connectivity")
            }
        }
    }

    async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let output = self
            .execute(
                &["api", "graphql", "--hostname", "github.com", "--input", "-"],
                Some(&json!({"query":query,"variables":variables})),
            )
            .await?;
        ensure!(
            output.get("errors").is_none(),
            "GitHub GraphQL returned errors or partial data; refusing an incomplete review snapshot"
        );
        output.get("data").cloned().context("Missing GraphQL data")
    }

    /// Lightweight identity/lifecycle lookup for ignored entries; no review polling.
    pub async fn ignored_details(&self, ids: &[String]) -> Result<Vec<Snapshot>> {
        let mut snapshots = Vec::new();
        for batch in ids.chunks(50) {
            let data = self.graphql("query IgnoredPrDetails($ids:[ID!]!){nodes(ids:$ids){... on PullRequest{id number title url state isDraft repository{nameWithOwner}}}}", json!({"ids":batch})).await?;
            for node in data["nodes"]
                .as_array()
                .context("Missing ignored PR identities")?
            {
                if node.is_null() {
                    continue;
                }
                let id = required(node, "id")?.to_owned();
                ensure!(batch.contains(&id), "Unexpected ignored PR identity");
                snapshots.push(Snapshot {
                    id,
                    repo: required(&node["repository"], "nameWithOwner")?.into(),
                    number: node["number"].as_u64().context("Missing PR number")?,
                    title: required(node, "title")?.into(),
                    url: required(node, "url")?.into(),
                    open: match required(node, "state")? {
                        "OPEN" => true,
                        "CLOSED" | "MERGED" => false,
                        _ => anyhow::bail!("Unknown ignored PR lifecycle state"),
                    },
                    draft: node["isDraft"].as_bool().unwrap_or(false),
                    ..Default::default()
                });
            }
        }
        Ok(snapshots)
    }

    pub async fn viewer(&self) -> Result<String> {
        let result = self
            .graphql("query { viewer { login } }", json!({}))
            .await?;
        Ok(required(&result["viewer"], "login")?.to_owned())
    }

    pub async fn discover(&self, login: &str) -> Result<Vec<PrRef>> {
        let mut found = BTreeMap::new();
        // Search can silently omit open PRs. The user's authored-PR connection
        // reads the underlying records directly and is not subject to search indexing.
        let mut cursor = Value::Null;
        loop {
            let data = self.graphql("query AuthoredPrs($cursor:String) { viewer { pullRequests(states:OPEN,first:100,after:$cursor) { pageInfo { hasNextPage endCursor } nodes { id number repository { nameWithOwner } } } } }", json!({"cursor":cursor})).await?;
            let connection = &data["viewer"]["pullRequests"];
            for node in nodes(connection)? {
                let id = required(node, "id")?.to_owned();
                found.insert(
                    id.clone(),
                    PrRef {
                        id,
                        repo: required(&node["repository"], "nameWithOwner")?.to_owned(),
                        number: node["number"].as_u64().context("Missing PR number")?,
                    },
                );
            }
            let Some(next) = next_cursor(connection)? else {
                break;
            };
            cursor = Value::String(next);
        }
        tracing::debug!(event = "authored_discovery_complete", count = found.len());
        for role in ["assignee", "review-requested"] {
            let mut cursor = Value::Null;
            loop {
                let data = self.graphql("query($q:String!,$cursor:String) { search(query:$q,type:ISSUE,first:100,after:$cursor) { issueCount pageInfo { hasNextPage endCursor } nodes { ... on PullRequest { id number repository { nameWithOwner } } } } }", json!({"q":format!("is:pr is:open {role}:{login}"),"cursor":cursor})).await?;
                let search = &data["search"];
                ensure!(
                    search["issueCount"].as_u64().unwrap_or(0) <= 1000,
                    "GitHub search exceeds 1,000 PRs; refusing a truncated discovery result"
                );
                for node in nodes(search)? {
                    let id = required(node, "id")?.to_owned();
                    found.insert(
                        id.clone(),
                        PrRef {
                            id,
                            repo: required(&node["repository"], "nameWithOwner")?.to_owned(),
                            number: node["number"].as_u64().context("Missing PR number")?,
                        },
                    );
                }
                let Some(next) = next_cursor(search)? else {
                    break;
                };
                cursor = Value::String(next);
            }
        }
        tracing::info!(event = "discovery_complete", count = found.len());
        Ok(found.into_values().collect())
    }

    pub async fn resolve_pr(&self, repo: &str, number: u64) -> Result<PrRef> {
        let (owner, name) = repo
            .split_once('/')
            .context("Repository must be OWNER/REPO")?;
        let data = self.graphql("query($owner:String!,$name:String!,$number:Int!) { repository(owner:$owner,name:$name) { pullRequest(number:$number) { id } } }", json!({"owner":owner,"name":name,"number":number})).await?;
        Ok(PrRef {
            id: required(&data["repository"]["pullRequest"], "id")?.to_owned(),
            repo: repo.into(),
            number,
        })
    }

    #[tracing::instrument(skip_all, fields(repo=%reference.repo, pr=reference.number))]
    pub async fn snapshot(&self, reference: &PrRef) -> Result<Snapshot> {
        let start = Instant::now();
        let mut cursors = [Value::Null, Value::Null, Value::Null, Value::Null];
        let mut done = [false; 4];
        let mut snapshot = Snapshot::default();
        let mut initial = true;
        // Independent connection cursors avoid truncating a busy PR or skipping nested pages.
        loop {
            let data = self.graphql(PR_QUERY, json!({"id":reference.id,"r":cursors[0],"c":cursors[1],"t":cursors[2],"e":cursors[3],"reviews":!done[0],"comments":!done[1],"threads":!done[2],"reactions":!done[3]})).await?;
            let pr = &data["node"];
            let head = required(pr, "headRefOid")?;
            if initial {
                snapshot = Snapshot {
                    id: reference.id.clone(),
                    repo: reference.repo.clone(),
                    number: reference.number,
                    title: required(pr, "title")?.into(),
                    url: required(pr, "url")?.into(),
                    head: head.into(),
                    open: pr["state"] == "OPEN",
                    draft: pr["isDraft"].as_bool().unwrap_or(false),
                    ..Default::default()
                };
                if !snapshot.open {
                    return Ok(snapshot);
                }
                initial = false;
            }
            ensure!(
                snapshot.head == head && pr["state"] == "OPEN",
                "PR changed during pagination; retrying next poll"
            );
            for (index, key) in ["reviews", "comments", "reviewThreads", "reactions"]
                .into_iter()
                .enumerate()
            {
                if done[index] {
                    continue;
                }
                let connection = &pr[key];
                for node in nodes(connection)? {
                    match index {
                        0 => snapshot.reviews.push(Review {
                            id: string(node, "id"),
                            author: string(&node["author"], "login"),
                            body: string(node, "body"),
                            state: string(node, "state"),
                            commit: string(&node["commit"], "oid"),
                            submitted_at: string(node, "submittedAt"),
                        }),
                        1 => snapshot.comments.push(Comment {
                            id: string(node, "id"),
                            author: string(&node["author"], "login"),
                            body: string(node, "body"),
                            updated_at: string(node, "updatedAt"),
                        }),
                        2 => {
                            let first = &node["first"]["nodes"][0];
                            let last = &node["last"]["nodes"][0];
                            snapshot.threads.push(Thread {
                                id: string(node, "id"),
                                resolved: node["isResolved"]
                                    .as_bool()
                                    .context("Missing thread resolution")?,
                                author: string(&first["author"], "login"),
                                last_comment_id: string(last, "id"),
                                updated_at: string(last, "updatedAt"),
                                body_hash: hash(format!(
                                    "{}{}",
                                    string(first, "body"),
                                    string(last, "body")
                                )),
                            });
                        }
                        3 => snapshot.reactions.push(Reaction {
                            id: string(node, "id"),
                            author: string(&node["user"], "login"),
                            content: string(node, "content"),
                            created_at: string(node, "createdAt"),
                        }),
                        _ => unreachable!(),
                    }
                }
                if let Some(cursor) = next_cursor(connection)? {
                    cursors[index] = Value::String(cursor);
                } else {
                    done[index] = true;
                }
            }
            if done.iter().all(|done| *done) {
                break;
            }
        }
        let mut page = 1;
        loop {
            let endpoint = format!(
                "repos/{}/commits/{}/check-runs?per_page=100&page={page}&filter=latest",
                reference.repo, snapshot.head
            );
            let data = self
                .execute(&["api", "--hostname", "github.com", &endpoint], None)
                .await?;
            let checks = data["check_runs"]
                .as_array()
                .context("Missing check runs")?;
            for check in checks {
                if Agent::from_app(&string(&check["app"], "slug")).is_none() {
                    continue;
                }
                snapshot.checks.push(Check {
                    id: check["id"].to_string(),
                    app: string(&check["app"], "slug"),
                    name: string(check, "name"),
                    status: string(check, "status"),
                    conclusion: string(check, "conclusion"),
                    started_at: string(check, "started_at"),
                    completed_at: string(check, "completed_at"),
                    summary: string(&check["output"], "summary"),
                });
            }
            if checks.len() < 100 {
                break;
            }
            page += 1;
        }
        // CodeRabbit also uses legacy commit statuses, which are separate from check runs.
        let mut statuses = Vec::new();
        let mut page = 1;
        loop {
            let endpoint = format!(
                "repos/{}/commits/{}/statuses?per_page=100&page={page}",
                reference.repo, snapshot.head
            );
            let data = self
                .execute(&["api", "--hostname", "github.com", &endpoint], None)
                .await?;
            let values = data.as_array().context("Missing commit statuses")?;
            statuses.extend(values.iter().cloned());
            if values.len() < 100 {
                break;
            }
            page += 1;
        }
        snapshot.checks.extend(legacy_checks(&statuses));
        let last = self
            .graphql(
                "query($id:ID!){ node(id:$id){ ... on PullRequest { headRefOid state } } }",
                json!({"id":reference.id}),
            )
            .await?;
        ensure!(
            last["node"]["headRefOid"] == snapshot.head && last["node"]["state"] == "OPEN",
            "PR changed while fetching checks; retrying next poll"
        );
        tracing::debug!(event="snapshot_complete", repo=%reference.repo, pr=reference.number, elapsed_ms=start.elapsed().as_millis() as u64, threads=snapshot.threads.len());
        Ok(snapshot)
    }
}

fn legacy_checks(statuses: &[Value]) -> Vec<Check> {
    let mut grouped: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for status in statuses {
        if Agent::from_login(&string(&status["creator"], "login")) == Some(Agent::CodeRabbit)
            && string(status, "context") == "CodeRabbit"
        {
            grouped.entry("CodeRabbit".into()).or_default().push(status);
        }
    }
    grouped
        .into_iter()
        .map(|(name, mut values)| {
            values.sort_by_key(|v| std::cmp::Reverse(v["id"].as_u64().unwrap_or(0)));
            let newest = values[0];
            let pending = string(newest, "state") == "pending";
            let started = if pending {
                newest
            } else {
                values
                    .get(1)
                    .filter(|v| string(v, "state") == "pending")
                    .copied()
                    .unwrap_or(newest)
            };
            Check {
                id: newest["id"].to_string(),
                app: "coderabbitai".into(),
                name,
                status: if pending { "in_progress" } else { "completed" }.into(),
                conclusion: if pending {
                    "".into()
                } else {
                    string(newest, "state")
                },
                started_at: string(started, "created_at"),
                completed_at: if pending {
                    String::new()
                } else {
                    string(newest, "created_at")
                },
                summary: string(newest, "description"),
            }
        })
        .collect()
}

struct RequestFailure {
    category: &'static str,
    http_status: Option<u16>,
    message: &'static str,
}

// Log only known classifications, never raw CLI stderr, headers, or response bodies.
fn classify_failure(stderr: &str) -> RequestFailure {
    static HTTP: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)\bHTTP\s+([1-5][0-9]{2})\b").unwrap());
    let http_status = HTTP.captures(stderr).and_then(|c| c[1].parse::<u16>().ok());
    let text = stderr.to_lowercase();
    let (category, message) = if http_status == Some(401)
        || ["bad credentials", "gh auth login", "authentication"]
            .iter()
            .any(|s| text.contains(s))
    {
        (
            "authentication",
            "GitHub authentication failed. Run gh auth login --hostname github.com.",
        )
    } else if http_status == Some(429) || text.contains("rate limit") {
        (
            "rate_limit",
            "GitHub rate limit reached; polling will back off.",
        )
    } else if matches!(http_status, Some(403 | 404)) {
        (
            "access",
            "GitHub access denied or repository unavailable; check repository permissions and organization SSO.",
        )
    } else if http_status.is_some_and(|s| s >= 500) {
        (
            "github_server",
            "GitHub reported a server error; polling will retry automatically.",
        )
    } else if text.contains("could not resolve")
        || text.contains("no such host")
        || text.contains("lookup ")
    {
        (
            "dns",
            "GitHub hostname could not be resolved; check network connectivity.",
        )
    } else if text.contains("x509") || text.contains("certificate") || text.contains("tls") {
        (
            "tls",
            "GitHub secure connection failed; check certificate and network settings.",
        )
    } else if text.contains("timeout")
        || text.contains("timed out")
        || text.contains("deadline exceeded")
    {
        ("timeout", "GitHub request timed out; check connectivity.")
    } else if [
        "connection reset",
        "connection refused",
        "network is unreachable",
        "unexpected eof",
        "broken pipe",
    ]
    .iter()
    .any(|s| text.contains(s))
    {
        (
            "connection",
            "GitHub connection was interrupted; polling will retry automatically.",
        )
    } else {
        (
            "unclassified",
            "GitHub request failed; check network connectivity and gh access.",
        )
    };
    RequestFailure {
        category,
        http_status,
        message,
    }
}

fn request_kind(args: &[&str], payload: Option<&Value>) -> &'static str {
    if let Some(query) = payload.and_then(|p| p["query"].as_str()) {
        if query.contains("search(") || query.contains("query AuthoredPrs") {
            "discover_prs"
        } else if query.contains("viewer {") {
            "viewer"
        } else if query.contains("reviews(first:") {
            "pr_details"
        } else if query.contains("headRefOid") {
            "verify_head"
        } else {
            "resolve_pr"
        }
    } else if args.iter().any(|a| a.contains("/check-runs?")) {
        "check_runs"
    } else if args.iter().any(|a| a.contains("/statuses?")) {
        "commit_statuses"
    } else {
        "rest"
    }
}

fn string(value: &Value, field: &str) -> String {
    value[field].as_str().unwrap_or("").to_owned()
}
fn required<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .with_context(|| format!("Missing GitHub field: {field}"))
}
fn nodes(connection: &Value) -> Result<&Vec<Value>> {
    connection["nodes"]
        .as_array()
        .context("Missing GitHub connection nodes")
}
fn next_cursor(connection: &Value) -> Result<Option<String>> {
    if connection["pageInfo"]["hasNextPage"]
        .as_bool()
        .context("Missing GitHub pagination metadata")?
    {
        Ok(Some(
            required(&connection["pageInfo"], "endCursor")?.to_owned(),
        ))
    } else {
        Ok(None)
    }
}

const PR_QUERY: &str = r#"
query($id:ID!,$r:String,$c:String,$t:String,$e:String,$reviews:Boolean!,$comments:Boolean!,$threads:Boolean!,$reactions:Boolean!) {
 node(id:$id) { ... on PullRequest {
  title url state isDraft headRefOid
  reviews(first:100,after:$r) @include(if:$reviews) {
   pageInfo { hasNextPage endCursor }
   nodes { id author { login } body state submittedAt commit { oid } }
  }
  comments(first:100,after:$c) @include(if:$comments) {
   pageInfo { hasNextPage endCursor }
   nodes { id author { login } body updatedAt }
  }
  reviewThreads(first:100,after:$t) @include(if:$threads) {
   pageInfo { hasNextPage endCursor }
   nodes { id isResolved first:comments(first:1) { nodes { author { login } body } } last:comments(last:1) { nodes { id body updatedAt } } }
  }
  reactions(first:100,after:$e) @include(if:$reactions) {
   pageInfo { hasNextPage endCursor }
   nodes { id user { login } content createdAt }
  }
 } }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn distinguishes_server_network_and_authentication_failures() {
        for (stderr, expected, status) in [
            ("gh: Bad Gateway (HTTP 502)", "github_server", Some(502)),
            (
                "gh: Bad credentials (HTTP 401)",
                "authentication",
                Some(401),
            ),
            (
                "gh: API rate limit exceeded (HTTP 403)",
                "rate_limit",
                Some(403),
            ),
            ("lookup api.github.com: no such host", "dns", None),
            ("x509: certificate signed by unknown authority", "tls", None),
            ("read: connection reset by peer", "connection", None),
            ("context deadline exceeded", "timeout", None),
        ] {
            let failure = classify_failure(stderr);
            assert_eq!(failure.category, expected);
            assert_eq!(failure.http_status, status);
        }
    }
    #[test]
    fn arbitrary_numbers_and_secrets_are_not_interpreted_or_logged() {
        let failure = classify_failure("request 40123 failed SECRET_TOKEN");
        assert_eq!(failure.category, "unclassified");
        assert_eq!(failure.http_status, None);
        assert!(!failure.message.contains("SECRET_TOKEN"));
    }
    #[test]
    fn legacy_statuses_use_latest_bot_result_and_original_run_start() {
        let statuses = vec![
            json!({"id":3,"context":"CodeRabbit","creator":{"login":"coderabbitai[bot]"},"state":"success","description":"Review completed","created_at":"2026-09-07T16:35:00Z"}),
            json!({"id":2,"context":"CodeRabbit","creator":{"login":"coderabbitai[bot]"},"state":"pending","description":"Review in progress","created_at":"2026-09-07T16:30:00Z"}),
            json!({"id":1,"context":"CodeRabbit","creator":{"login":"coderabbitai[bot]"},"state":"success","description":"Review skipped","created_at":"2026-09-07T16:00:00Z"}),
        ];
        let checks = legacy_checks(&statuses);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, "completed");
        assert_eq!(checks[0].started_at, "2026-09-07T16:30:00Z");
        assert_eq!(checks[0].summary, "Review completed");
    }
    #[test]
    fn legacy_statuses_cannot_impersonate_an_agent_by_context_alone() {
        assert!(legacy_checks(&[json!({"id":1,"context":"CodeRabbit","creator":{"login":"someone"},"state":"pending"})]).is_empty());
    }
}
