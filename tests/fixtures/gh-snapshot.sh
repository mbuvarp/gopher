# Invoked as a mock GitHub CLI by integration tests; no network access.
case "$*" in
  *check-runs*) echo '{"check_runs":[]}'; exit 0 ;;
  *statuses*) echo '[]'; exit 0 ;;
esac
input=$(cat)
case "$input" in
  *'"r":"next"'*)
    echo '{"data":{"node":{"state":"OPEN","headRefOid":"head","reviews":{"nodes":[{"id":"review-2","author":{"login":"cubic-dev-ai[bot]"},"body":"No issues found","state":"COMMENTED","submittedAt":"2026-09-07T16:00:00Z","commit":{"oid":"head"}}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}' ;;
  *'reviews(first:100'*)
    echo '{"data":{"node":{"title":"Example","url":"https://github.com/owner/repo/pull/1","state":"OPEN","isDraft":false,"headRefOid":"head","reviews":{"nodes":[{"id":"review-1","author":{"login":"cubic-dev-ai[bot]"},"body":"2 issues found","state":"COMMENTED","submittedAt":"2026-09-07T15:00:00Z","commit":{"oid":"head"}}],"pageInfo":{"hasNextPage":true,"endCursor":"next"}},"comments":{"nodes":[],"pageInfo":{"hasNextPage":false}},"reviewThreads":{"nodes":[{"id":"thread-1","isResolved":false,"first":{"nodes":[{"author":{"login":"cubic-dev-ai[bot]"},"body":"Finding"}]},"last":{"nodes":[{"id":"comment-1","body":"Finding","updatedAt":"2026-09-07T15:00:00Z"}]}}],"pageInfo":{"hasNextPage":false}},"reactions":{"nodes":[{"id":"reaction-1","user":{"login":"someone"},"content":"EYES","createdAt":"2026-09-07T15:00:00Z"}],"pageInfo":{"hasNextPage":false}}}}}' ;;
  *) echo '{"data":{"node":{"headRefOid":"head","state":"OPEN"}}}' ;;
esac
