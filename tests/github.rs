#![cfg(unix)]
use gopher::{config::Config, github::Github};
use std::os::unix::fs::PermissionsExt;

fn mock(script: &str) -> (tempfile::TempDir, Github) {
    mock_with_timeout(script, 10)
}
fn mock_with_timeout(script: &str, timeout: u64) -> (tempfile::TempDir, Github) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gh");
    std::fs::write(&path, format!("#!/bin/sh\nif [ \"$1\" = auth ]; then echo test-credential; exit 0; fi\nset -eu\n{script}\n")).unwrap();
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
async fn merging_sends_the_exact_head_and_selected_method_and_reports_blockers() {
    use gopher::{actions::MergeMethod, github::PrRef};
    let pr = PrRef {
        id: "PR_1".into(),
        repo: "owner/repo".into(),
        number: 42,
    };
    for method in MergeMethod::ALL {
        let (_dir, gh) = mock(&format!(
            r#"
case "$*" in *'--method PUT repos/owner/repo/pulls/42/merge --input -'*) ;; *) exit 1;; esac
input=$(cat)
case "$input" in *'"merge_method":"{}"'*'"sha":"expected-head"'*) echo '{{"merged":true}}';; *) exit 1;; esac
"#,
            method.api_name()
        ));
        gh.merge_pr(&pr, "expected-head", method).await.unwrap();
        assert!(gh.merge_pr(&pr, "", method).await.is_err());
    }
    let (_dir, gh) = mock(
        "cat >/dev/null\necho '{\"message\":\"Required status check is pending\"}'\necho 'HTTP 405 PRIVATE_STDERR' >&2\nexit 1",
    );
    let error = gh
        .merge_pr(&pr, "expected-head", MergeMethod::Merge)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("Required status check is pending"));
    assert!(!error.contains("PRIVATE_STDERR"));
    let (_dir, gh) =
        mock("cat >/dev/null\necho '{\"merged\":false,\"message\":\"Head branch was modified\"}'");
    assert!(
        gh.merge_pr(&pr, "expected-head", MergeMethod::Merge)
            .await
            .unwrap_err()
            .to_string()
            .contains("Head branch was modified")
    );
}

#[tokio::test]
async fn label_changes_are_incremental_and_escape_names_as_one_path_segment() {
    let pr = gopher::github::PrRef {
        id: "PR_1".into(),
        repo: "owner/repo".into(),
        number: 42,
    };
    let (_dir, gh) = mock(
        r#"
case "$*" in
  *'--method POST repos/owner/repo/issues/42/labels --input -'*)
    input=$(cat)
    [ "$input" = '{"labels":["a/b, c"]}' ] || exit 1
    echo '[]';;
  *'--method DELETE repos/owner/repo/issues/42/labels/a%2Fb,%20c'*) echo '[]';;
  *) exit 1;;
esac
"#,
    );
    gh.set_label(&pr, "a/b, c", true).await.unwrap();
    gh.set_label(&pr, "a/b, c", false).await.unwrap();
}

#[tokio::test]
async fn repository_labels_are_fully_paginated_without_fetching_assignments() {
    let first: Vec<_> = (0..100)
        .map(|i| serde_json::json!({"name":format!("label-{i:03}"),"color":"ff0011"}))
        .collect();
    let first = serde_json::to_string(&first).unwrap();
    let (_dir, gh) = mock(&format!(
        r#"
case "$*" in
  *'/issues/'*) exit 1;;
  *'&page=1'*) echo '{first}';;
  *'&page=2'*) echo '[{{"name":"last","color":"123456"}}]';;
  *) exit 1;;
esac
"#
    ));
    let labels = gh.repository_labels("owner/repo").await.unwrap();
    assert_eq!(labels.len(), 101);
    assert_eq!(labels.last().unwrap().color, "123456");
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
    assert_eq!(
        snapshot
            .labels
            .iter()
            .map(|l| (l.name.as_str(), l.color.as_str()))
            .collect::<Vec<_>>(),
        vec![("bug", "d73a4a"), ("ready", "008800")]
    );
}
#[tokio::test]
async fn rejects_head_changes_during_pagination() {
    let script = include_str!("fixtures/gh-snapshot.sh").replace(
        "\"headRefOid\":\"head\",\"labels\":{\"nodes\":[{\"name\":\"ready\"",
        "\"headRefOid\":\"new-head\",\"labels\":{\"nodes\":[{\"name\":\"ready\"",
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

#[tokio::test]
async fn rest_etags_reuse_bodies_across_clients_and_mutations_invalidate_them() {
    let (dir, github) = mock(
        r#"
case "$*" in
  *graphql*) cat >/dev/null; echo '{"data":{"viewer":{"login":"test"}}}'; exit 0;;
  *'--method DELETE'*) printf 'HTTP/2.0 204 No Content\r\n\r\n'; exit 0;;
esac
printf '%s\n' "$*" >> "$(dirname "$0")/calls"
case "$*" in
  *If-None-Match*) printf 'HTTP/2.0 304 Not Modified\r\nETag: "abc"\r\n\r\n'; echo 'HTTP 304' >&2; exit 1;;
  *) printf 'HTTP/2.0 200 OK\r\nETag: "abc"\r\n\r\n[{"name":"bug","color":"112233"}]';;
esac
"#,
    );
    github.viewer().await.unwrap();
    let labels = github.repository_labels("owner/repo").await.unwrap();
    assert_eq!(labels[0].name, "bug");
    let config = Config {
        gh_path: Some(dir.path().join("gh")),
        ..Default::default()
    };
    let next = Github::new(&config).unwrap();
    next.viewer().await.unwrap();
    assert_eq!(next.repository_labels("owner/repo").await.unwrap(), labels);
    next.set_label(
        &gopher::github::PrRef {
            id: "PR_1".into(),
            repo: "owner/repo".into(),
            number: 1,
        },
        "bug",
        false,
    )
    .await
    .unwrap();
    next.repository_labels("owner/repo").await.unwrap();
    let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
    let calls: Vec<_> = calls.lines().collect();
    assert_eq!(calls.len(), 3);
    assert!(!calls[0].contains("If-None-Match"));
    assert!(calls[1].contains("If-None-Match: \"abc\""));
    assert!(!calls[2].contains("If-None-Match"));
}

#[tokio::test]
async fn secondary_limit_pauses_other_clients_without_retrying_a_mutation() {
    let (dir, github) = mock(
        r#"
cat >/dev/null
printf 'call\n' >> "$(dirname "$0")/calls"
printf 'HTTP/2.0 403 Forbidden\r\nRetry-After: 180\r\n\r\n{"message":"Secondary rate limit reached"}'
echo 'HTTP 403 SECRET' >&2
exit 1
"#,
    );
    let error = github.viewer().await.unwrap_err().to_string();
    assert!(error.contains("rate limit"));
    assert!(!error.contains("SECRET"));
    let config = Config {
        gh_path: Some(dir.path().join("gh")),
        ..Default::default()
    };
    assert!(Github::cooldown(&config, None) >= std::time::Duration::from_secs(179));
    let next = Github::new(&config).unwrap();
    assert!(next.repository_labels("owner/repo").await.is_err());
    assert!(
        next.merge_pr(
            &gopher::github::PrRef {
                id: "PR_1".into(),
                repo: "owner/repo".into(),
                number: 1
            },
            "head",
            gopher::actions::MergeMethod::Merge
        )
        .await
        .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[tokio::test]
async fn graphql_primary_limit_in_a_successful_http_response_still_pauses_requests() {
    let reset = chrono::Utc::now().timestamp() + 600;
    let (dir, github) = mock(&format!(
        r#"
cat >/dev/null
printf 'HTTP/2.0 200 OK\r\nX-RateLimit-Resource: graphql\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: {reset}\r\n\r\n{{"errors":[{{"type":"RATE_LIMITED","message":"quota reached"}}]}}'
"#
    ));
    assert!(
        github
            .viewer()
            .await
            .unwrap_err()
            .to_string()
            .contains("rate limit")
    );
    let config = Config {
        gh_path: Some(dir.path().join("gh")),
        ..Default::default()
    };
    let expected = reset
        .saturating_sub(chrono::Utc::now().timestamp() + 1)
        .max(0) as u64;
    assert!(Github::cooldown(&config, None) >= std::time::Duration::from_secs(expected));
}

#[tokio::test]
async fn account_switch_escapes_old_cooldowns_and_switching_back_preserves_them() {
    let reset = chrono::Utc::now().timestamp() + 1800;
    // Override the standard fake credential prelude so gh auth switch is local.
    let (dir, github) = mock("exit 1");
    let script = format!(
        r#"#!/bin/sh
if [ "$1" = auth ]; then cat "$(dirname "$0")/account"; exit 0; fi
cat >/dev/null
printf '%s %s\n' "$GH_TOKEN" "$*" >> "$(dirname "$0")/calls"
if [ "$GH_TOKEN" = alpha ]; then
 printf 'HTTP/2.0 200 OK\r\nX-RateLimit-Resource: graphql\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: {reset}\r\n\r\n{{"data":{{"viewer":{{"login":"alice"}}}}}}'
else
 echo '{{"data":{{"viewer":{{"login":"bob"}}}}}}'
fi
"#
    );
    std::fs::write(dir.path().join("gh"), script).unwrap();
    std::fs::write(dir.path().join("account"), "alpha").unwrap();
    assert_eq!(github.viewer().await.unwrap(), "alice");
    let config = Config {
        gh_path: Some(dir.path().join("gh")),
        ..Default::default()
    };
    assert!(Github::cooldown(&config, Some("graphql")).as_secs() > 1700);
    assert!(Github::cooldown(&config, Some("core")).is_zero());
    std::fs::write(dir.path().join("account"), "beta").unwrap();
    let next = Github::new(&config).unwrap();
    assert_eq!(next.viewer().await.unwrap(), "bob");
    assert!(Github::cooldown(&config, None).is_zero());
    // A client already authenticated as B keeps that credential for its mutation.
    std::fs::write(dir.path().join("account"), "alpha").unwrap();
    next.set_label(
        &gopher::github::PrRef {
            id: "PR_1".into(),
            repo: "owner/repo".into(),
            number: 1,
        },
        "bug",
        true,
    )
    .await
    .unwrap();
    assert!(
        std::fs::read_to_string(dir.path().join("calls"))
            .unwrap()
            .lines()
            .last()
            .unwrap()
            .starts_with("beta ")
    );
    // A fresh read discovers A locally, then blocks its exhausted GraphQL quota.
    assert!(
        Github::new(&config)
            .unwrap()
            .ignored_details(&["PR_1".into()])
            .await
            .is_err()
    );
    assert!(Github::cooldown(&config, Some("graphql")).as_secs() > 1700);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("calls"))
            .unwrap()
            .lines()
            .count(),
        3
    );
}

#[tokio::test]
async fn graphql_limit_allows_rest_label_refresh_and_core_limit_allows_ignored_lookup() {
    let reset = chrono::Utc::now().timestamp() + 1800;
    let (dir, github) = mock(&format!(
        r#"
cat >/dev/null
case "$*" in
 *IgnoredPrDetails*) exit 99;;
 *graphql*) printf 'HTTP/2.0 200 OK\r\nX-RateLimit-Resource: graphql\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: {reset}\r\n\r\n{{"data":{{"viewer":{{"login":"test"}}}}}}';;
 *'github.com user'*) echo '{{"login":"test"}}';;
 *'/labels?'*) echo '[{{"name":"bug","color":"ff0000"}}]';;
 *) exit 1;;
esac
"#
    ));
    github.viewer().await.unwrap();
    let config = Config {
        gh_path: Some(dir.path().join("gh")),
        ..Default::default()
    };
    let labels = Github::new(&config).unwrap();
    assert_eq!(labels.viewer().await.unwrap(), "test");
    assert_eq!(
        labels.repository_labels("owner/repo").await.unwrap()[0].name,
        "bug"
    );
    assert_eq!(labels.viewer().await.unwrap(), "test");
    assert!(Github::cooldown(&config, Some("core")).is_zero());

    let (dir, github) = mock(&format!(
        r#"
input=$(cat)
case "$*" in
 *'/labels?'*) printf 'HTTP/2.0 200 OK\r\nX-RateLimit-Resource: core\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: {reset}\r\n\r\n[]';;
 *) echo '{{"data":{{"nodes":[],"viewer":{{"login":"test"}}}}}}';;
esac
"#
    ));
    github.repository_labels("owner/repo").await.unwrap();
    let config = Config {
        gh_path: Some(dir.path().join("gh")),
        ..Default::default()
    };
    assert!(Github::cooldown(&config, Some("core")).as_secs() > 1700);
    assert!(Github::cooldown(&config, Some("graphql")).is_zero());
    github.ignored_details(&["PR_1".into()]).await.unwrap();
    github.viewer().await.unwrap();
}
