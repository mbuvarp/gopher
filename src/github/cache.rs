//! Shared per-executable request metadata. Response bodies are bounded and never logged.
use super::*;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Default)]
pub(super) struct ApiState {
    account: Option<String>,
    generation: u64,
    cached: BTreeMap<String, Cached>,
    bytes: usize,
    blocked: BTreeMap<String, Instant>,
    secondary_failures: u32,
}
#[derive(Clone)]
pub(super) struct Cached {
    pub etag: String,
    pub body: Vec<u8>,
}
#[derive(Clone)]
pub(super) struct CacheKey {
    endpoint: String,
    account: String,
    generation: u64,
}

pub(super) fn shared(path: &std::path::Path) -> Arc<Mutex<ApiState>> {
    static STATES: OnceLock<Mutex<BTreeMap<PathBuf, Arc<Mutex<ApiState>>>>> = OnceLock::new();
    let mut states = STATES.get_or_init(Default::default).lock().unwrap();
    if states.len() >= 64 {
        states.retain(|_, state| Arc::strong_count(state) > 1);
    }
    states.entry(path.to_owned()).or_default().clone()
}
impl ApiState {
    pub fn identify(&mut self, account: &str) {
        if self.account.as_deref() != Some(account) {
            self.account = Some(account.into());
            self.generation += 1;
            self.cached.clear();
            self.bytes = 0;
        }
    }
    pub fn key(&self, endpoint: &str, account: Option<&str>) -> Option<CacheKey> {
        let account = account.filter(|a| self.account.as_deref() == Some(*a))?;
        Some(CacheKey {
            endpoint: endpoint.into(),
            account: account.into(),
            generation: self.generation,
        })
    }
    fn valid(&self, key: &CacheKey) -> bool {
        self.account.as_deref() == Some(&key.account) && self.generation == key.generation
    }
    pub fn get(&self, key: &CacheKey) -> Option<Cached> {
        self.valid(key)
            .then(|| self.cached.get(&key.endpoint).cloned())
            .flatten()
    }
    pub fn put(&mut self, key: &CacheKey, etag: Option<&str>, body: &[u8]) {
        if !self.valid(key) {
            return;
        }
        if let Some(old) = self.cached.remove(&key.endpoint) {
            self.bytes -= old.body.len();
        }
        let Some(etag) = etag else {
            return;
        };
        if body.len() > 1024 * 1024 || etag.len() > 1024 {
            return;
        }
        if self.bytes + body.len() > 16 * 1024 * 1024 || self.cached.len() >= 256 {
            self.cached.clear();
            self.bytes = 0;
        }
        self.bytes += body.len();
        self.cached.insert(
            key.endpoint.clone(),
            Cached {
                etag: etag.into(),
                body: body.to_vec(),
            },
        );
    }
    pub fn invalidate(&mut self) {
        // Prevent an older in-flight GET from repopulating the cache after a mutation.
        self.generation += 1;
        self.cached.clear();
        self.bytes = 0;
    }
    pub fn delay(&self, resource: Option<&str>) -> Duration {
        self.blocked
            .iter()
            .filter(|(key, _)| {
                resource.is_none_or(|r| key.as_str() == r || key.as_str() == "secondary")
            })
            .map(|(_, until)| until.saturating_duration_since(Instant::now()))
            .max()
            .unwrap_or_default()
    }
    pub fn observe(&mut self, headers: &BTreeMap<String, String>, resource: &str, limited: bool) {
        let actual_resource = headers
            .get("x-ratelimit-resource")
            .map(String::as_str)
            .unwrap_or(resource);
        let number = |key: &str| headers.get(key).and_then(|v| v.parse::<u64>().ok());
        if let (Some(limit), Some(remaining)) =
            (number("x-ratelimit-limit"), number("x-ratelimit-remaining"))
        {
            tracing::info!(
                event = "github_rate_limit",
                resource = actual_resource,
                limit,
                remaining,
                used = number("x-ratelimit-used"),
                reset = number("x-ratelimit-reset")
            );
        }
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        if number("x-ratelimit-remaining") == Some(0) {
            let seconds = number("x-ratelimit-reset")
                .map(|v| v.saturating_sub(now).saturating_add(1))
                .unwrap_or(60);
            self.block(actual_resource, seconds.max(1));
        }
        if !limited && self.delay(None).is_zero() {
            self.secondary_failures = 0;
        }
        if limited {
            // Prefer Retry-After; primary limits also retain their resource reset above.
            let seconds = number("retry-after").or_else(|| {
                headers
                    .get("retry-after")
                    .and_then(|v| chrono::DateTime::parse_from_rfc2822(v).ok())
                    .map(|v| {
                        (v.timestamp().max(0) as u64)
                            .saturating_sub(now)
                            .saturating_add(1)
                    })
            });
            if let Some(seconds) = seconds {
                self.block("secondary", seconds.max(1));
            } else if number("x-ratelimit-remaining") != Some(0) {
                self.secondary_failures = self.secondary_failures.saturating_add(1);
                self.block(
                    "secondary",
                    60 * 2u64.pow(self.secondary_failures.min(5) - 1),
                );
            }
        }
    }
    fn block(&mut self, resource: &str, seconds: u64) {
        // Guard malformed headers against Instant overflow, without shortening valid resets.
        let until = Instant::now() + Duration::from_secs(seconds.min(31_536_000));
        self.blocked
            .entry(resource.into())
            .and_modify(|old| *old = (*old).max(until))
            .or_insert(until);
        tracing::warn!(
            event = "github_rate_limited",
            resource,
            retry_after_seconds = seconds
        );
    }
}

/// gh --include emits HTTP status/headers before the body. Headerless responses
/// remain supported for test CLIs. Skip informational/proxy header blocks.
pub(super) fn response(bytes: &[u8]) -> Result<(u16, BTreeMap<String, String>, &[u8])> {
    let mut rest = bytes;
    let mut status = 200;
    let mut headers = BTreeMap::new();
    while rest.starts_with(b"HTTP/") {
        let (end, separator) = rest
            .windows(4)
            .position(|v| v == b"\r\n\r\n")
            .map(|i| (i, 4))
            .or_else(|| rest.windows(2).position(|v| v == b"\n\n").map(|i| (i, 2)))
            .context("Missing GitHub HTTP header separator")?;
        let text = std::str::from_utf8(&rest[..end]).context("Invalid GitHub HTTP headers")?;
        let mut lines = text.lines();
        status = lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .context("Invalid GitHub HTTP status")?;
        headers.clear();
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().into());
            }
        }
        rest = &rest[end + separator..];
    }
    Ok((status, headers, rest))
}

pub(super) fn is_rate_limit(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("rate limit") || message.contains("abuse detection")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_headers_and_empty_304_without_reading_body_as_headers() {
        let (status, headers, body) = response(b"HTTP/1.1 200 Connection established\r\n\r\nHTTP/2.0 304 Not Modified\r\nETag: W/\"a\"\r\nX-RateLimit-Remaining: 42\r\n\r\n").unwrap();
        assert_eq!(status, 304);
        assert_eq!(headers["etag"], "W/\"a\"");
        assert_eq!(headers["x-ratelimit-remaining"], "42");
        assert!(body.is_empty());
        assert_eq!(
            response(b"HTTP/2.0 200 OK\nETag: x\n\n[]").unwrap().2,
            b"[]"
        );
        assert!(response(b"HTTP/2.0 200 OK").is_err());
    }

    #[test]
    fn cache_requires_verified_account_and_invalidates_in_flight_keys() {
        let mut state = ApiState::default();
        assert!(state.key("repos/a/b/labels", None).is_none());
        state.identify("alice");
        let key = state.key("repos/a/b/labels", Some("alice")).unwrap();
        state.put(&key, Some("first"), b"[]");
        assert!(state.get(&key).is_some());
        state.invalidate();
        state.put(&key, Some("late"), b"[]");
        assert!(state.get(&key).is_none());
        let key = state.key("repos/a/b/labels", Some("alice")).unwrap();
        state.put(&key, Some("second"), b"[]");
        state.identify("bob");
        assert!(state.get(&key).is_none());
        assert!(state.key("repos/a/b/labels", Some("alice")).is_none());
        state.put(&key, Some("late"), b"[]");
        assert!(state.cached.is_empty());
    }

    #[test]
    fn response_cache_bounds_memory_and_does_not_retain_unvalidated_bodies() {
        let mut state = ApiState::default();
        state.identify("alice");
        let key = state.key("repos/a/b/labels", Some("alice")).unwrap();
        state.put(&key, Some("x"), &vec![b' '; 1024 * 1024 + 1]);
        assert!(state.get(&key).is_none());
        state.put(&key, Some("x"), b"[]");
        state.put(&key, None, b"[]");
        assert!(state.get(&key).is_none());
        assert_eq!(state.bytes, 0);
    }

    #[test]
    fn rate_limits_respect_resource_reset_retry_after_and_exponential_cooldown() {
        let mut state = ApiState::default();
        let reset = (chrono::Utc::now().timestamp() + 600).to_string();
        state.observe(
            &BTreeMap::from([
                ("x-ratelimit-resource".into(), "graphql".into()),
                ("x-ratelimit-remaining".into(), "0".into()),
                ("x-ratelimit-reset".into(), reset),
            ]),
            "graphql",
            true,
        );
        assert!(state.delay(Some("graphql")) >= Duration::from_secs(599));
        assert!(state.delay(Some("core")).is_zero());
        state.observe(
            &BTreeMap::from([("retry-after".into(), "120".into())]),
            "core",
            true,
        );
        assert!(state.delay(Some("core")) >= Duration::from_secs(119));
        let mut state = ApiState::default();
        state.observe(&BTreeMap::new(), "core", true);
        assert!(state.delay(None) >= Duration::from_secs(59));
        state.blocked.clear();
        state.observe(&BTreeMap::new(), "core", true);
        assert!(state.delay(None) >= Duration::from_secs(119));
        state.blocked.clear();
        state.observe(&BTreeMap::new(), "core", false);
        assert_eq!(state.secondary_failures, 0);
    }
}

#[cfg(all(test, unix))]
mod head_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn final_heads_are_batched_and_reject_missing_or_changed_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gh");
        let ids: Vec<_> = (0..51).map(|i| format!("PR_{i}")).collect();
        let nodes = |ids: &[String]| {
            ids.iter()
                .map(|id| json!({"id":id,"headRefOid":"head","state":"OPEN"}))
                .collect::<Vec<_>>()
        };
        let first = json!({"data":{"viewer":{"login":"test"},"nodes":nodes(&ids[..50])}});
        let last = json!({"data":{"viewer":{"login":"test"},"nodes":nodes(&ids[50..])}});
        let script = format!(
            r#"#!/bin/sh
input=$(cat)
echo call >> "$(dirname "$0")/calls"
case "$input" in *'"PR_50"'*) echo '{last}';; *) echo '{first}';; esac
"#
        );
        std::fs::write(&path, &script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let gh = Github::new(&Config {
            gh_path: Some(path.clone()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(gh.final_heads(&ids, "test").await.unwrap().len(), 51);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("calls"))
                .unwrap()
                .lines()
                .count(),
            2
        );
        assert!(
            gh.final_heads(&ids, "other")
                .await
                .unwrap_err()
                .to_string()
                .contains("account changed")
        );
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\ncat >/dev/null\necho '{}'",
                json!({"data":{"viewer":{"login":"test"},"nodes":[null]}})
            ),
        )
        .unwrap();
        assert!(gh.final_heads(&ids[50..], "test").await.is_err());
    }
}
