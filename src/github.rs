use crate::{config::Config, model::*};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::{io::AsyncWriteExt, process::Command};
mod check_status;

#[derive(Clone)]
pub struct Github {
    executable: PathBuf,
    timeout: Duration,
    session: std::sync::Arc<std::sync::Mutex<Option<credentials::Session>>>,
    account: std::sync::Arc<std::sync::Mutex<Option<String>>>,
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
        let executable = resolve_gh(config)?;
        Ok(Self {
            session: Default::default(),
            account: Default::default(),
            executable,
            timeout: Duration::from_secs(config.request_timeout_seconds),
        })
    }

    pub fn cooldown(config: &Config, resource: Option<&str>) -> Duration {
        resolve_gh(config)
            .ok()
            .and_then(|path| cache::active(&path))
            .map(|api| api.lock().unwrap().delay(resource))
            .unwrap_or_default()
    }

    async fn execute(&self, args: &[&str], payload: Option<&Value>) -> Result<Value> {
        self.execute_with_missing_nodes(args, payload, false).await
    }

    async fn execute_with_missing_nodes(
        &self,
        args: &[&str],
        payload: Option<&Value>,
        allow_missing_nodes: bool,
    ) -> Result<Value> {
        self.execute_response(
            args,
            payload,
            if allow_missing_nodes {
                ResponsePolicy::MissingNodes
            } else {
                ResponsePolicy::Strict
            },
        )
        .await
    }

    async fn execute_response(
        &self,
        args: &[&str],
        payload: Option<&Value>,
        policy: ResponsePolicy,
    ) -> Result<Value> {
        let start = Instant::now();
        let request = request_kind(args, payload);
        let resource = if args.contains(&"graphql") {
            "graphql"
        } else {
            "core"
        };
        let session = self.session().await?;
        let api = &session.api;
        let delay = api.lock().unwrap().delay(Some(resource));
        ensure!(
            delay.is_zero(),
            "GitHub rate limit: requests paused until the quota cooldown ends"
        );
        let endpoint = args.iter().find(|a| a.starts_with("repos/"));
        let key = if payload.is_none() && !matches!(policy, ResponsePolicy::Mutation) {
            endpoint.and_then(|endpoint| {
                api.lock()
                    .unwrap()
                    .key(endpoint, self.account.lock().unwrap().as_deref())
            })
        } else {
            None
        };
        let cached = key.as_ref().and_then(|key| api.lock().unwrap().get(key));
        let mut command = Command::new(&self.executable);
        command
            .args(args)
            .arg("--include")
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_PAGER", "cat")
            .env("GH_HOST", "github.com")
            .env("GH_TOKEN", &session.token)
            .env_remove("GH_DEBUG")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(cached) = &cached {
            command.args(["-H", &format!("If-None-Match: {}", cached.etag)]);
        }
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
            ensure!(
                output.stdout.len() <= 32 * 1024 * 1024,
                "GitHub response exceeded 32 MiB"
            );
            let (status, headers, body) = cache::response(&output.stdout)?;
            tracing::debug!(
                event = "github_request",
                request,
                resource,
                http_status = status,
                elapsed_ms = start.elapsed().as_millis() as u64,
                conditional = cached.is_some(),
                success = output.status.success() || status == 304
            );

            let parsed = if status == 204 {
                Ok(Value::Null)
            } else {
                serde_json::from_slice::<Value>(body)
            };
            let failure = classify_failure(&String::from_utf8_lossy(&output.stderr));
            let limited = status == 429
                || (status == 403 && headers.contains_key("retry-after"))
                || failure.category == "rate_limit"
                || parsed.as_ref().is_ok_and(|v| {
                    v["message"].as_str().is_some_and(cache::is_rate_limit)
                        || v["errors"].as_array().is_some_and(|errors| {
                            errors.iter().any(|e| {
                                e["type"] == "RATE_LIMITED"
                                    || e["message"].as_str().is_some_and(cache::is_rate_limit)
                            })
                        })
                });
            // Service-unavailable responses can also ask all requests to wait.
            // Preserve the service error below instead of calling it a quota error.
            {
                let mut state = api.lock().unwrap();
                state.observe(&headers, resource, limited);
                if status == 503 {
                    state.observe_service_retry(&headers);
                }
            }
            if limited {
                bail!("GitHub rate limit reached; requests paused until the quota cooldown ends");
            }
            if status == 304 {
                let cached = cached.context("GitHub returned 304 without a cached response")?;
                ensure!(
                    key.as_ref()
                        .is_some_and(|key| api.lock().unwrap().get(key).is_some()),
                    "GitHub response cache changed during the request; retry next poll"
                );
                tracing::debug!(event = "github_not_modified", resource);
                return serde_json::from_slice(&cached.body).context("Invalid cached GitHub JSON");
            }
            // gh exits nonzero for GraphQL node-resolution errors, even when
            // unrelated nodes succeeded. Only the ignored-identity lookup may
            // accept these narrowly validated partial responses.
            if matches!(policy, ResponsePolicy::MissingNodes)
                && let Ok(response) = &parsed
                && only_missing_node_errors(response)
            {
                return Ok(response.clone());
            }
            if !output.status.success() || status >= 400 {
                tracing::warn!(
                    event = "github_request_failed",
                    request,
                    category = failure.category,
                    reason = failure.reason,
                    http_status = failure.http_status,
                    exit_code = output.status.code(),
                    elapsed_ms = start.elapsed().as_millis() as u64
                );
                if matches!(policy, ResponsePolicy::Mutation)
                    && let Ok(response) = &parsed
                    && response["message"].is_string()
                {
                    bail!(
                        "GitHub: {}",
                        actions::message(response, "Action was rejected")
                    );
                }
                bail!("{}", failure.message);
            }
            let response = parsed.context("GitHub CLI returned invalid JSON")?;
            if matches!(policy, ResponsePolicy::Mutation) {
                api.lock().unwrap().invalidate();
            } else if status == 200
                && response.get("errors").is_none()
                && let Some(key) = &key
            {
                api.lock()
                    .unwrap()
                    .put(key, headers.get("etag").map(String::as_str), body);
            }
            Ok(response)
        };
        match tokio::time::timeout(self.timeout, operation).await {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!(
                    event = "github_request_failed",
                    request,
                    category = "timeout",
                    reason = "process_timeout",
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
            let response = self.execute_with_missing_nodes(
                &["api", "graphql", "--hostname", "github.com", "--input", "-"],
                Some(&json!({"query":"query IgnoredPrDetails($ids:[ID!]!){nodes(ids:$ids){... on PullRequest{id number title url state isDraft headRefName baseRefName isCrossRepository headRepositoryOwner{login} repository{nameWithOwner}}}}", "variables":{"ids":batch}})),
                true,
            ).await?;
            ensure!(
                response.get("errors").is_none() || only_missing_node_errors(&response),
                "GitHub GraphQL returned errors during ignored PR lookup"
            );
            let data = &response["data"];
            if let Some(errors) = response["errors"].as_array() {
                tracing::warn!(event = "ignored_prs_unavailable", count = errors.len());
            }
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
                    head_branch: node["headRefName"].as_str().map(str::to_owned),
                    base_branch: node["baseRefName"].as_str().map(str::to_owned),
                    source_owner: (node["isCrossRepository"] == true)
                        .then(|| {
                            node["headRepositoryOwner"]["login"]
                                .as_str()
                                .map(str::to_owned)
                        })
                        .flatten(),
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
        // This local lookup observes gh auth switch even while the previous account
        // is rate limited; it never sends a request to GitHub.
        let session = self.refresh_credentials().await?;
        let use_rest = {
            let api = session.api.lock().unwrap();
            !api.delay(Some("graphql")).is_zero() && api.delay(Some("core")).is_zero()
        };
        let account = if use_rest {
            let result = self
                .execute(&["api", "--hostname", "github.com", "user"], None)
                .await?;
            required(&result, "login")?.to_owned()
        } else {
            let result = self
                .graphql("query { viewer { login } }", json!({}))
                .await?;
            required(&result["viewer"], "login")?.to_owned()
        };
        session.api.lock().unwrap().identify(&account);
        *self.account.lock().unwrap() = Some(account.clone());
        Ok(account)
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
        let snapshot = self.snapshot_unverified(reference).await?;
        if snapshot.open {
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
        }
        Ok(snapshot)
    }

    pub(crate) async fn final_heads(
        &self,
        ids: &[String],
        viewer: &str,
    ) -> Result<BTreeMap<String, (String, bool)>> {
        let mut heads = BTreeMap::new();
        for batch in ids.chunks(50) {
            // A poll may span a local `gh auth switch`. Recheck credentials for
            // each batch so its viewer reflects the currently selected account.
            self.refresh_credentials().await?;
            let data = self.graphql("query PollHeads($ids:[ID!]!){viewer{login} nodes(ids:$ids){... on PullRequest{id headRefOid state}}}", json!({"ids":batch})).await?;
            ensure!(
                required(&data["viewer"], "login")? == viewer,
                "GitHub account changed while refreshing PRs"
            );
            let nodes = data["nodes"]
                .as_array()
                .context("Missing final PR head checks")?;
            ensure!(
                nodes.len() == batch.len(),
                "Incomplete final PR head checks"
            );
            for (id, node) in batch.iter().zip(nodes) {
                ensure!(
                    node["id"] == *id,
                    "Missing or unexpected PR identity in final head checks"
                );
                let open = match required(node, "state")? {
                    "OPEN" => true,
                    "CLOSED" | "MERGED" => false,
                    _ => bail!("Unknown PR lifecycle state"),
                };
                heads.insert(id.clone(), (required(node, "headRefOid")?.into(), open));
            }
        }
        Ok(heads)
    }

    /// Polling validates these results together with final_heads before publishing.
    #[tracing::instrument(skip_all, fields(repo=%reference.repo, pr=reference.number))]
    pub(crate) async fn snapshot_unverified(&self, reference: &PrRef) -> Result<Snapshot> {
        let start = Instant::now();
        let mut cursors = [
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ];
        let mut done = [false; 5];
        let mut snapshot = Snapshot::default();
        let mut initial = true;
        // Independent connection cursors avoid truncating a busy PR or skipping nested pages.
        loop {
            let data = self.graphql(PR_QUERY, json!({"id":reference.id,"r":cursors[0],"c":cursors[1],"t":cursors[2],"e":cursors[3],"reviews":!done[0],"comments":!done[1],"threads":!done[2],"reactions":!done[3],"l":cursors[4],"labels":!done[4]})).await?;
            let pr = &data["node"];
            let head = required(pr, "headRefOid")?;
            if initial {
                snapshot = Snapshot {
                    id: reference.id.clone(),
                    repo: reference.repo.clone(),
                    number: reference.number,
                    title: required(pr, "title")?.into(),
                    head_branch: pr["headRefName"].as_str().map(str::to_owned),
                    base_branch: pr["baseRefName"].as_str().map(str::to_owned),
                    source_owner: (pr["isCrossRepository"] == true)
                        .then(|| {
                            pr["headRepositoryOwner"]["login"]
                                .as_str()
                                .map(str::to_owned)
                        })
                        .flatten(),
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
            // Use the latest page: GitHub may finish calculating mergeability
            // while connections are being paginated. Unknown falls back to CI.
            snapshot.check_state = Some(if pr["mergeable"] == "CONFLICTING" {
                CheckState::Conflicts
            } else {
                CheckState::Green
            });
            for (index, key) in [
                "reviews",
                "comments",
                "reviewThreads",
                "reactions",
                "labels",
            ]
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
                        4 => snapshot.labels.push(crate::model::PrLabel {
                            name: required(node, "name")?.into(),
                            color: required(node, "color")?.into(),
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
        let mut check_state = snapshot.check_state.unwrap_or_default();
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
                check_state = check_state.max(check_status::check_run(check));
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
        snapshot.check_state = Some(check_state.max(check_status::commit_statuses(&statuses)));
        tracing::debug!(event="snapshot_collected", repo=%reference.repo, pr=reference.number, elapsed_ms=start.elapsed().as_millis() as u64, threads=snapshot.threads.len());
        Ok(snapshot)
    }
}

/// Every error must identify an unavailable root node, never an incomplete
/// field on a returned PR or a query-wide authentication/server failure.
fn only_missing_node_errors(response: &Value) -> bool {
    let Some(nodes) = response["data"]["nodes"].as_array() else {
        return false;
    };
    let Some(errors) = response["errors"].as_array().filter(|e| !e.is_empty()) else {
        return false;
    };
    errors.iter().all(|error| {
        error["type"] == "NOT_FOUND"
            && error["path"].as_array().is_some_and(|path| {
                path.len() == 2
                    && path[0] == "nodes"
                    && path[1].as_u64().is_some_and(|index| {
                        usize::try_from(index)
                            .ok()
                            .and_then(|index| nodes.get(index))
                            .is_some_and(Value::is_null)
                    })
            })
    })
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
    reason: &'static str,
    http_status: Option<u16>,
    message: &'static str,
}

// Log only known classifications, never raw CLI stderr, headers, or response bodies.
fn classify_failure(stderr: &str) -> RequestFailure {
    static HTTP: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)\bHTTP\s+([1-5][0-9]{2})\b").unwrap());
    let http_status = HTTP.captures(stderr).and_then(|c| c[1].parse::<u16>().ok());
    let text = stderr.to_lowercase();
    let (category, reason, message) = if http_status == Some(401)
        || ["bad credentials", "gh auth login", "authentication"]
            .iter()
            .any(|s| text.contains(s))
    {
        (
            "authentication",
            "authentication_failed",
            "GitHub authentication failed. Run gh auth login --hostname github.com.",
        )
    } else if http_status == Some(429) || text.contains("rate limit") {
        (
            "rate_limit",
            "rate_limited",
            "GitHub rate limit reached; polling will back off.",
        )
    } else if matches!(http_status, Some(403 | 404)) {
        (
            "access",
            "access_denied",
            "GitHub access denied or repository unavailable; check repository permissions and organization SSO.",
        )
    } else if http_status.is_some_and(|s| s >= 500) {
        (
            "github_server",
            "server_error",
            "GitHub reported a server error; polling will retry automatically.",
        )
    } else if text.contains("could not resolve")
        || text.contains("no such host")
        || text.contains("lookup ")
    {
        (
            "dns",
            "dns_resolution_failed",
            "GitHub hostname could not be resolved; check network connectivity.",
        )
    } else if text.contains("tls handshake timeout") || text.contains("tls handshake timed out") {
        (
            "timeout",
            "tls_handshake_timeout",
            "GitHub secure connection timed out; try again shortly.",
        )
    } else if text.contains("x509") || text.contains("certificate") {
        (
            "tls",
            "certificate_verification_failed",
            "GitHub certificate verification failed; check certificate and network settings.",
        )
    } else if text.contains("timeout")
        || text.contains("timed out")
        || text.contains("deadline exceeded")
    {
        (
            "timeout",
            "request_timeout",
            "GitHub request timed out; check connectivity.",
        )
    } else if text.contains("tls") {
        (
            "tls",
            "tls_failure",
            "GitHub secure connection failed; try again or check network settings.",
        )
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
            "connection_interrupted",
            "GitHub connection was interrupted; polling will retry automatically.",
        )
    } else {
        (
            "unclassified",
            "unclassified",
            "GitHub request failed; check network connectivity and gh access.",
        )
    };
    RequestFailure {
        category,
        reason,
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
query($id:ID!,$r:String,$c:String,$t:String,$e:String,$reviews:Boolean!,$comments:Boolean!,$threads:Boolean!,$reactions:Boolean!,$l:String,$labels:Boolean!) {
 node(id:$id) { ... on PullRequest {
  title url state isDraft headRefOid mergeable headRefName baseRefName isCrossRepository
  headRepositoryOwner { login }
  labels(first:100,after:$l) @include(if:$labels) {
   pageInfo { hasNextPage endCursor }
   nodes { name color }
  }
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
    fn distinguishes_tls_timeouts_certificate_failures_and_other_tls_errors() {
        for (stderr, category, reason) in [
            (
                "Get https://api.github.com/private: net/http: TLS handshake timeout SECRET_TOKEN",
                "timeout",
                "tls_handshake_timeout",
            ),
            (
                "TLS handshake timed out",
                "timeout",
                "tls_handshake_timeout",
            ),
            (
                "TLS handshake: context deadline exceeded",
                "timeout",
                "request_timeout",
            ),
            ("dial tcp: i/o timeout", "timeout", "request_timeout"),
            (
                "tls: failed to verify certificate: x509: certificate signed by unknown authority SECRET_TOKEN",
                "tls",
                "certificate_verification_failed",
            ),
            (
                "x509: certificate has expired or is not yet valid",
                "tls",
                "certificate_verification_failed",
            ),
            ("remote error: tls: handshake failure", "tls", "tls_failure"),
            (
                "gh: TLS handshake timeout (HTTP 502)",
                "github_server",
                "server_error",
            ),
        ] {
            let failure = classify_failure(stderr);
            assert_eq!(failure.category, category, "{stderr}");
            assert_eq!(failure.reason, reason, "{stderr}");
            assert!(!failure.message.contains("SECRET_TOKEN"));
            assert!(!failure.message.contains("/private"));
            if category == "timeout" || reason == "tls_failure" {
                assert!(!failure.message.contains("certificate"));
            }
        }
    }
    #[test]
    fn arbitrary_numbers_and_secrets_are_not_interpreted_or_logged() {
        let failure = classify_failure("request 40123 failed SECRET_TOKEN");
        assert_eq!(failure.category, "unclassified");
        assert_eq!(failure.http_status, None);
        assert!(!failure.message.contains("SECRET_TOKEN"));
        assert_eq!(failure.reason, "unclassified");
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
mod actions;

#[derive(Clone, Copy)]
enum ResponsePolicy {
    Strict,
    MissingNodes,
    Mutation,
}

mod cache;

mod credentials;
