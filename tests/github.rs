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
input=$(cat)
case "$input" in
  *AuthoredPrs*) echo '{"data":{"viewer":{"pullRequests":{"pageInfo":{"hasNextPage":false},"nodes":[{"id":"PR_1","number":1,"repository":{"nameWithOwner":"owner/repo"}}]}}}}' ;;
  *assignee:*) echo '{"data":{"search":{"issueCount":2,"pageInfo":{"hasNextPage":false},"nodes":[{"id":"PR_1","number":1,"repository":{"nameWithOwner":"owner/repo"}},{"id":"PR_2","number":2,"repository":{"nameWithOwner":"owner/repo"}}]}}}' ;;
  *review-requested:*) echo '{"data":{"search":{"issueCount":1,"pageInfo":{"hasNextPage":false},"nodes":[{"id":"PR_3","number":3,"repository":{"nameWithOwner":"owner/repo"}}]}}}' ;;
  *) exit 1 ;;
esac
"#,
    );
    let refs = gh.discover("someone").await.unwrap();
    assert_eq!(
        refs.iter().map(|r| r.number).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(refs[0].repo, "owner/repo");
}
#[tokio::test]
async fn authored_discovery_paginates_even_when_search_returns_nothing() {
    let (_dir, gh) = mock(
        r#"
input=$(cat)
case "$input" in
  *'"cursor":"next"'*) echo '{"data":{"viewer":{"pullRequests":{"pageInfo":{"hasNextPage":false},"nodes":[{"id":"PR_2","number":2,"repository":{"nameWithOwner":"owner/repo"}}]}}}}' ;;
  *AuthoredPrs*) echo '{"data":{"viewer":{"pullRequests":{"pageInfo":{"hasNextPage":true,"endCursor":"next"},"nodes":[{"id":"PR_1","number":1,"repository":{"nameWithOwner":"owner/repo"}}]}}}}' ;;
  *) echo '{"data":{"search":{"issueCount":0,"pageInfo":{"hasNextPage":false},"nodes":[]}}}' ;;
esac
"#,
    );
    let refs = gh.discover("someone").await.unwrap();
    assert_eq!(
        refs.iter().map(|r| r.number).collect::<Vec<_>>(),
        vec![1, 2]
    );
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

#[tokio::test]
async fn ignored_identity_lookup_handles_unavailable_and_closed_prs() {
    let (_dir, gh) = mock(
        r#"
cat >/dev/null
echo '{"data":{"nodes":[null,{"id":"PR_2","number":42,"title":"Archived change","url":"https://github.com/owner/repo/pull/42","state":"CLOSED","isDraft":false,"repository":{"nameWithOwner":"owner/repo"}}]}}'
"#,
    );
    let found = gh
        .ignored_details(&["unavailable".into(), "PR_2".into()])
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].number, 42);
    assert!(!found[0].open);
    assert!(found[0].reviews.is_empty());
    assert!(gh.ignored_details(&["different-id".into()]).await.is_err());
}

#[tokio::test]
async fn ignored_lookup_accepts_missing_nodes_even_when_gh_exits_nonzero() {
    for exit in [0, 1] {
        let (_dir, gh) = mock(&format!(
            r#"
cat >/dev/null
echo '{{"data":{{"nodes":[null,{{"id":"PR_2","number":42,"title":"Archived change","url":"https://github.com/owner/repo/pull/42","state":"CLOSED","isDraft":false,"repository":{{"nameWithOwner":"owner/repo"}}}}]}},"errors":[{{"type":"NOT_FOUND","path":["nodes",0],"message":"Unavailable SECRET"}}]}}'
echo 'gh: Unavailable SECRET' >&2
exit {exit}
"#
        ));
        let found = gh
            .ignored_details(&["unavailable".into(), "PR_2".into()])
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "PR_2");
        assert!(!found[0].open);
        // The same partial response must still fail for ordinary GraphQL requests.
        let error = gh.viewer().await.unwrap_err().to_string();
        assert!(!error.contains("SECRET"));

        let cache = tempfile::tempdir().unwrap();
        let mut store = gopher::store::Store::open(cache.path()).unwrap();
        for id in ["unavailable", "PR_2"] {
            let pr = gopher::model::PullRequest::unreviewed(gopher::model::Snapshot {
                id: id.into(),
                open: true,
                ..Default::default()
            });
            store.save(&pr).unwrap();
            store.ignore(id).unwrap();
        }
        for snapshot in found {
            store.update_ignored_status(&snapshot).unwrap();
        }
        let visible = store.load_ignored().unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].snapshot.id, "unavailable");
        assert_eq!(store.ignored().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn unavailable_later_batch_preserves_previously_fetched_identities() {
    let (_dir, gh) = mock(
        r#"
input=$(cat)
case "$input" in
  *PR_0*) echo '{"data":{"nodes":[{"id":"PR_0","number":42,"title":"Open change","url":"https://github.com/owner/repo/pull/42","state":"OPEN","isDraft":false,"repository":{"nameWithOwner":"owner/repo"}}]}}' ;;
  *) echo '{"data":{"nodes":[null]},"errors":[{"type":"NOT_FOUND","path":["nodes",0]}]}'; exit 1 ;;
esac
"#,
    );
    let ids = (0..51).map(|n| format!("PR_{n}")).collect::<Vec<_>>();
    let found = gh.ignored_details(&ids).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, "PR_0");
    assert!(found[0].open);
    assert!(
        gh.ignored_details(&["PR_50".into()])
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn ignored_lookup_still_rejects_query_and_field_errors() {
    for error in [
        r#"{"type":"FORBIDDEN","path":["nodes",0]}"#,
        r#"{"type":"NOT_FOUND"}"#,
        r#"{"type":"NOT_FOUND","path":["nodes",1]}"#,
        r#"{"type":"NOT_FOUND","path":["nodes",0,"state"]}"#,
    ] {
        for exit in [0, 1] {
            let (_dir, gh) = mock(&format!(
                "cat >/dev/null\necho '{{\"data\":{{\"nodes\":[null]}},\"errors\":[{error}]}}'\nexit {exit}"
            ));
            assert!(gh.ignored_details(&["PR_1".into()]).await.is_err());
        }
    }
}
