#![cfg(unix)]
use gopher::{config::Config, github::Github};
use std::os::unix::fs::PermissionsExt;

fn mock(script: &str) -> (tempfile::TempDir, Github) {
    mock_with_timeout(script, 10)
}
fn mock_with_timeout(script: &str, timeout: u64) -> (tempfile::TempDir, Github) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gh");
    std::fs::write(&path, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let github = Github::new(&Config {
        gh_path: Some(path),
        request_timeout_seconds: timeout,
        ..Default::default()
    })
    .unwrap();
    (dir, github)
}
#[tokio::test]
async fn authentication_failure_is_actionable_and_does_not_leak_stderr() {
    let (_dir, gh) = mock("cat >/dev/null\necho 'bad credentials (HTTP 401) SECRET' >&2\nexit 1");
    let error = gh.viewer().await.unwrap_err().to_string();
    assert!(error.contains("gh auth login"));
    assert!(!error.contains("SECRET"));
}
#[tokio::test]
async fn refuses_partial_graphql_data() {
    let (_dir, gh) = mock(
        "cat >/dev/null\necho '{\"data\":{\"viewer\":{\"login\":\"someone\"}},\"errors\":[{\"message\":\"Partial\"}]}'",
    );
    assert!(
        gh.viewer()
            .await
            .unwrap_err()
            .to_string()
            .contains("partial data")
    );
}
#[tokio::test]
async fn requests_timeout() {
    let (_dir, gh) = mock_with_timeout("cat >/dev/null\nexec sleep 5", 1);
    let start = std::time::Instant::now();
    assert!(
        gh.viewer()
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out")
    );
    assert!(start.elapsed().as_secs() < 3);
}
#[tokio::test]
async fn discovery_unions_roles_and_deduplicates() {
    let (_dir, gh) = mock(
        r#"
cat >/dev/null
echo '{"data":{"search":{"issueCount":1,"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"id":"PR_1","number":1,"repository":{"nameWithOwner":"owner/repo"}}]}}}'
"#,
    );
    let refs = gh.discover("someone").await.unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].repo, "owner/repo");
}
#[tokio::test]
async fn snapshot_paginates_reviews_independently_and_checks_head() {
    let (_dir, gh) = mock(include_str!("fixtures/gh-snapshot.sh"));
    let pr = gopher::github::PrRef {
        id: "PR_1".into(),
        repo: "owner/repo".into(),
        number: 1,
    };
    let snapshot = gh.snapshot(&pr).await.unwrap();
    assert_eq!(snapshot.reviews.len(), 2);
    assert_eq!(snapshot.reviews[1].id, "review-2");
    assert_eq!(snapshot.threads.len(), 1);
    assert!(!snapshot.threads[0].resolved);
    assert_eq!(snapshot.reactions.len(), 1);
}
#[tokio::test]
async fn rejects_head_changes_during_pagination() {
    let script = include_str!("fixtures/gh-snapshot.sh").replace(
        "\"headRefOid\":\"head\",\"reviews\":{\"nodes\":[{\"id\":\"review-2\"",
        "\"headRefOid\":\"new-head\",\"reviews\":{\"nodes\":[{\"id\":\"review-2\"",
    );
    let (_dir, gh) = mock(&script);
    let pr = gopher::github::PrRef {
        id: "PR_1".into(),
        repo: "owner/repo".into(),
        number: 1,
    };
    assert!(
        gh.snapshot(&pr)
            .await
            .unwrap_err()
            .to_string()
            .contains("changed during pagination")
    );
}
