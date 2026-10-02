//! Request metadata scoped to a CLI executable and credential fingerprint. Response bodies are bounded and never logged.
use super::*;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Default)]
pub(super) struct ApiState {
    account: Option<String>,
    generation: u64,
    cached: BTreeMap<Arc<str>, Entry>,
    /// Endpoints by last use, oldest first, sharing the keys of `cached`.
    recency: BTreeMap<u64, Arc<str>>,
    bytes: usize,
    clock: u64,
    limits: Limits,
    blocked: BTreeMap<String, Instant>,
    secondary_failures: u32,
}
fn retry_after(headers: &BTreeMap<String, String>) -> Option<u64> {
    let value = headers.get("retry-after")?;
    value.parse().ok().or_else(|| {
        chrono::DateTime::parse_from_rfc2822(value)
            .ok()
            .map(|date| {
                date.timestamp()
                    .saturating_sub(chrono::Utc::now().timestamp())
                    .saturating_add(1)
                    .max(0) as u64
            })
    })
}

#[derive(Clone)]
pub(super) struct Cached {
    pub etag: String,
    /// Shared so reading an entry under the lock never copies its body.
    pub body: Arc<[u8]>,
}
struct Entry {
    cached: Cached,
    used: u64,
}
/// Counts every retained string so the byte bound covers ETags and keys too.
fn size(endpoint: &str, etag: &str, body: &[u8]) -> usize {
    endpoint.len() + etag.len() + body.len()
}
/// Polls revisit every entry in a fixed order, so least-recently-used eviction
/// only saves requests while the cap exceeds the working set; bytes bound memory.
#[derive(Clone, Copy)]
struct Limits {
    entries: usize,
    bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            entries: 2048,
            bytes: 16 * 1024 * 1024,
        }
    }
}
#[derive(Clone)]
pub(super) struct CacheKey {
    endpoint: String,
    account: String,
    generation: u64,
}

#[derive(Default)]
struct Profiles {
    active: String,
    states: BTreeMap<String, Arc<Mutex<ApiState>>>,
}
fn profiles() -> &'static Mutex<BTreeMap<PathBuf, Profiles>> {
    static PROFILES: OnceLock<Mutex<BTreeMap<PathBuf, Profiles>>> = OnceLock::new();
    PROFILES.get_or_init(Default::default)
}
pub(super) fn active(path: &std::path::Path) -> Option<Arc<Mutex<ApiState>>> {
    let profiles = profiles().lock().unwrap();
    let profiles = profiles.get(path)?;
    profiles.states.get(&profiles.active).cloned()
}
pub(super) fn credential(path: &std::path::Path, fingerprint: &str) -> Arc<Mutex<ApiState>> {
    let mut profiles = profiles().lock().unwrap();
    if profiles.len() >= 64 {
        profiles.retain(|_, profile| {
            profile.states.values().any(|state| {
                Arc::strong_count(state) > 1 || !state.lock().unwrap().delay(None).is_zero()
            })
        });
    }
    let profile = profiles.entry(path.to_owned()).or_default();
    if profile.active != fingerprint {
        if let Some(previous) = profile.states.get(&profile.active) {
            // Keep the old credential's quota deadline for switching back, but
            // discard its response bodies and invalidate any in-flight cache writes.
            previous.lock().unwrap().invalidate();
        }
        profile.states.retain(|_, state| {
            Arc::strong_count(state) > 1 || !state.lock().unwrap().delay(None).is_zero()
        });
        profile.active = fingerprint.into();
    }
    profile
        .states
        .entry(fingerprint.into())
        .or_default()
        .clone()
}
impl ApiState {
    pub fn identify(&mut self, account: &str) {
        if self.account.as_deref() != Some(account) {
            self.account = Some(account.into());
            self.generation += 1;
            self.clear();
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
        self.same_account(key) && self.generation == key.generation
    }
    /// A 304 confirms the body read when the request started, even if a
    /// credential switch invalidated the cache meanwhile; only an identity change voids it.
    pub fn same_account(&self, key: &CacheKey) -> bool {
        self.account.as_deref() == Some(&key.account)
    }
    pub fn get(&mut self, key: &CacheKey) -> Option<Cached> {
        if !self.valid(key) {
            return None;
        }
        let entry = self.cached.get_mut(key.endpoint.as_str())?;
        self.clock += 1;
        let Some(endpoint) = self.recency.remove(&entry.used) else {
            self.reset_inconsistent();
            return None;
        };
        entry.used = self.clock;
        let cached = entry.cached.clone();
        self.recency.insert(self.clock, endpoint);
        Some(cached)
    }
    pub fn put(&mut self, key: &CacheKey, etag: Option<&str>, body: &[u8]) {
        if !self.valid(key) {
            return;
        }
        self.remove(&key.endpoint);
        let Some(etag) = etag else {
            return;
        };
        let needed = size(&key.endpoint, etag, body);
        if body.len() > 1024 * 1024 || etag.len() > 1024 || needed > self.limits.bytes {
            return;
        }
        // Evict individually so one overflow cannot discard every conditional request;
        // entries for superseded heads are never read again and go first.
        let mut evicted = 0;
        while self.bytes + needed > self.limits.bytes || self.cached.len() >= self.limits.entries {
            let Some((_, oldest)) = self.recency.pop_first() else {
                // Never panic under the shared lock; dropping the cache only costs requests.
                self.reset_inconsistent();
                break;
            };
            self.remove(&oldest);
            evicted += 1;
        }
        self.clock += 1;
        let endpoint: Arc<str> = key.endpoint.as_str().into();
        self.bytes += needed;
        self.recency.insert(self.clock, endpoint.clone());
        self.cached.insert(
            endpoint,
            Entry {
                cached: Cached {
                    etag: etag.into(),
                    body: body.into(),
                },
                used: self.clock,
            },
        );
        if evicted > 0 {
            tracing::debug!(
                event = "github_cache_evicted",
                evicted,
                entries = self.cached.len(),
                bytes = self.bytes
            );
        }
    }
    fn remove(&mut self, endpoint: &str) {
        if let Some(old) = self.cached.remove(endpoint) {
            self.recency.remove(&old.used);
            let size = size(endpoint, &old.cached.etag, &old.cached.body);
            self.bytes = self.bytes.saturating_sub(size);
        }
    }
    fn reset_inconsistent(&mut self) {
        tracing::warn!(
            event = "github_cache_inconsistent",
            entries = self.cached.len()
        );
        self.clear();
    }
    fn clear(&mut self) {
        self.cached.clear();
        self.recency.clear();
        self.bytes = 0;
    }
    pub fn invalidate(&mut self) {
        // Prevent an older in-flight GET from repopulating the cache after a credential switch.
        self.generation += 1;
        self.clear();
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
        let primary = number("x-ratelimit-remaining") == Some(0);
        if primary {
            let seconds = number("x-ratelimit-reset")
                .map(|v| v.saturating_sub(now).saturating_add(1))
                .unwrap_or(60);
            self.block(actual_resource, seconds.max(1));
        }
        if !limited && self.delay(None).is_zero() {
            self.secondary_failures = 0;
        }
        if limited {
            // Honor both deadlines without promoting a primary quota into a
            // shared secondary limit when GitHub also supplies Retry-After.
            if let Some(seconds) = retry_after(headers) {
                let scope = if primary {
                    actual_resource
                } else {
                    "secondary"
                };
                self.block(scope, seconds.max(1));
            } else if !primary {
                self.secondary_failures = self.secondary_failures.saturating_add(1);
                self.block(
                    "secondary",
                    60 * 2u64.pow(self.secondary_failures.min(5) - 1),
                );
            }
        }
    }
    pub fn observe_service_retry(&mut self, headers: &BTreeMap<String, String>) {
        // An outage applies across resources even if quota headers are also present.
        if let Some(seconds) = retry_after(headers) {
            self.block("secondary", seconds.max(1));
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
        assert!(state.same_account(&key));
        state.put(&key, Some("late"), b"[]");
        assert!(state.get(&key).is_none());
        let key = state.key("repos/a/b/labels", Some("alice")).unwrap();
        state.put(&key, Some("second"), b"[]");
        state.identify("bob");
        assert!(state.get(&key).is_none());
        assert!(!state.same_account(&key));
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
    fn overflow_evicts_least_recently_used_entries_instead_of_clearing() {
        let mut state = ApiState {
            limits: Limits {
                entries: 3,
                bytes: 16,
            },
            ..Default::default()
        };
        state.identify("alice");
        let key = |state: &ApiState, endpoint: &str| state.key(endpoint, Some("alice")).unwrap();
        let [a, b, c, d, e, f, g] =
            ["a", "b", "c", "d", "e", "f", "g"].map(|endpoint| key(&state, endpoint));
        // Each entry counts its endpoint, ETag, and body: 1 + 1 + 2 bytes.
        for key in [&a, &b, &c] {
            state.put(key, Some("x"), b"12");
        }
        // Reading keeps an entry alive, so the untouched oldest one is evicted.
        // Assertions inspect the map directly so they do not reorder recency.
        let cached = |state: &ApiState, keys: &[&str]| {
            keys.iter()
                .map(|key| state.cached.contains_key(*key))
                .collect::<Vec<_>>()
        };
        assert!(state.get(&a).is_some());
        state.put(&d, Some("x"), b"12");
        assert_eq!(
            cached(&state, &["a", "b", "c", "d"]),
            [true, false, true, true]
        );
        assert_eq!((state.cached.len(), state.bytes), (3, 12));
        // Replacing an entry reuses its space without evicting others.
        state.put(&a, Some("y"), b"123");
        assert_eq!(state.cached["a"].cached.etag, "y");
        assert_eq!((state.cached.len(), state.bytes), (3, 13));
        // A large body evicts only as many of the oldest entries as needed to fit.
        state.put(&e, Some("x"), b"12345");
        assert_eq!(
            cached(&state, &["a", "c", "d", "e"]),
            [true, false, true, true]
        );
        assert_eq!((state.cached.len(), state.bytes), (3, 16));
        // Entries larger than the whole cache are never retained or evict others.
        state.put(&f, Some("x"), b"123456789012345");
        assert_eq!(cached(&state, &["f"]), [false]);
        assert_eq!((state.cached.len(), state.bytes), (3, 16));
        // ETags count toward the bound even when the body is empty.
        state.put(&g, Some("012345"), b"");
        assert_eq!(
            cached(&state, &["a", "d", "e", "g"]),
            [false, false, true, true]
        );
        assert_eq!((state.cached.len(), state.bytes), (2, 14));
        assert_eq!(state.recency.len(), state.cached.len());
    }

    #[test]
    fn inconsistent_recency_resets_the_cache_instead_of_panicking() {
        let mut state = ApiState {
            limits: Limits {
                entries: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        state.identify("alice");
        let a = state.key("a", Some("alice")).unwrap();
        let b = state.key("b", Some("alice")).unwrap();
        state.put(&a, Some("x"), b"[]");
        state.recency.clear();
        assert!(state.get(&a).is_none());
        assert_eq!((state.cached.len(), state.bytes), (0, 0));
        state.put(&a, Some("x"), b"[]");
        state.recency.clear();
        state.put(&b, Some("x"), b"[]");
        assert!(state.get(&b).is_some());
        assert_eq!(
            (state.cached.len(), state.recency.len(), state.bytes),
            (1, 1, 4)
        );
    }

    #[test]
    fn polls_keep_current_heads_cached_while_superseded_heads_are_evicted() {
        let mut state = ApiState::default();
        state.identify("alice");
        // 300 PRs with three endpoints per head, polled in a fixed order; one PR
        // pushes a new head every poll, leaving its previous entries unused. Label
        // catalogues for 20 repositories refresh every 30 polls (about 15 minutes).
        let mut heads = [0; 300];
        for poll in 0..1000 {
            heads[poll % heads.len()] += 1;
            let mut misses = 0;
            let labels = poll % 30 == 0;
            for repo in (0..20).filter(|_| labels) {
                let path = format!("repos/a/{repo}/labels");
                let key = state.key(&path, Some("alice")).unwrap();
                if state.get(&key).is_none() {
                    misses += 1;
                    state.put(&key, Some("x"), b"[]");
                }
            }
            for (pr, head) in heads.iter().enumerate() {
                for endpoint in ["check-runs", "statuses", "runs"] {
                    let path = format!("repos/a/b/{pr}/{head}/{endpoint}");
                    let key = state.key(&path, Some("alice")).unwrap();
                    if state.get(&key).is_none() {
                        misses += 1;
                        state.put(&key, Some("x"), b"[]");
                    }
                }
            }
            // After the first poll, only the new head is fetched unconditionally.
            assert_eq!(misses, if poll == 0 { 920 } else { 3 }, "poll {poll}");
        }
        assert_eq!(state.cached.len(), 2048);
        assert_eq!(state.recency.len(), 2048);
    }

    #[test]
    fn primary_retry_after_preserves_resource_isolation_and_the_longer_deadline() {
        let now = chrono::Utc::now();
        for (resource, other) in [("core", "graphql"), ("graphql", "core")] {
            for (retry_after, seconds) in [
                ("120".to_owned(), 120),
                ("1200".to_owned(), 1200),
                ((now + chrono::Duration::seconds(1200)).to_rfc2822(), 1200),
            ] {
                let mut state = ApiState::default();
                state.observe(
                    &BTreeMap::from([
                        ("x-ratelimit-resource".into(), resource.into()),
                        ("x-ratelimit-remaining".into(), "0".into()),
                        (
                            "x-ratelimit-reset".into(),
                            (now.timestamp() + 600).to_string(),
                        ),
                        ("retry-after".into(), retry_after),
                    ]),
                    resource,
                    true,
                );
                assert!(state.delay(Some(resource)) >= Duration::from_secs(seconds.max(600) - 2));
                assert!(state.delay(Some(other)).is_zero());
                assert!(!state.blocked.contains_key("secondary"));
                assert_eq!(state.secondary_failures, 0);
            }
        }
        // Secondary limits must still pause both resources.
        let mut state = ApiState::default();
        state.observe(
            &BTreeMap::from([
                ("x-ratelimit-remaining".into(), "100".into()),
                ("retry-after".into(), "120".into()),
            ]),
            "core",
            true,
        );
        for resource in ["core", "graphql"] {
            assert!(state.delay(Some(resource)) >= Duration::from_secs(119));
        }
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
    async fn final_heads_refresh_credentials_before_each_batch() {
        for switch_before_check in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("gh");
            let account = dir.path().join("account");
            let ids: Vec<_> = (0..51).map(|i| format!("PR_{i}")).collect();
            let nodes = |ids: &[String]| {
                serde_json::to_string(
                    &ids.iter()
                        .map(|id| json!({"id":id,"headRefOid":"head","state":"OPEN"}))
                        .collect::<Vec<_>>(),
                )
                .unwrap()
            };
            let first = nodes(&ids[..50]);
            let last = nodes(&ids[50..]);
            std::fs::write(
                &path,
                format!(
                    r#"#!/bin/sh
set -eu
if [ "$1" = auth ]; then cat "$(dirname "$0")/account"; exit 0; fi
input=$(cat)
echo "$GH_TOKEN" >> "$(dirname "$0")/calls"
nodes='[]'
case "$input" in
    *PollHeads*)
        case "$input" in *'"PR_50"'*) nodes='{last}';; *) nodes='{first}';; esac
        echo beta > "$(dirname "$0")/account"
        ;;
esac
printf '{{"data":{{"viewer":{{"login":"%s"}},"nodes":%s}}}}' "$GH_TOKEN" "$nodes"
"#
                ),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::write(&account, "alpha").unwrap();
            let gh = Github::new(&Config {
                gh_path: Some(path),
                ..Default::default()
            })
            .unwrap();
            assert_eq!(gh.viewer().await.unwrap(), "alpha");
            if switch_before_check {
                std::fs::write(&account, "beta").unwrap();
            }
            assert!(
                gh.final_heads(&ids, "alpha")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("account changed")
            );
            let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
            assert_eq!(
                calls,
                if switch_before_check {
                    "alpha\nbeta\n"
                } else {
                    "alpha\nalpha\nbeta\n"
                }
            );
        }
    }

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
if [ "$1" = auth ]; then echo test-credential; exit 0; fi
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
                "#!/bin/sh\nif [ \"$1\" = auth ]; then echo test-credential; exit 0; fi\ncat >/dev/null\necho '{}'",
                json!({"data":{"viewer":{"login":"test"},"nodes":[null]}})
            ),
        )
        .unwrap();
        assert!(gh.final_heads(&ids[50..], "test").await.is_err());
    }
}
