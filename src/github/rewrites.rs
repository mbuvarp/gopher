//! Compare a reviewed commit with a later head, so a review can survive a
//! rewrite that keeps the PR's changes, such as a clean rebase onto the base.
use super::*;

/// Up to 300 files are listed for one comparison; a full list is required.
const MAX_FILES: usize = 300;

impl Github {
    /// Compares each reviewed commit with the head. Failures never fail the
    /// snapshot: they leave the review unverified for the current head.
    pub(super) async fn reviewed_commits(
        &self,
        repo: &str,
        base: &str,
        head: &str,
        commits: Vec<String>,
    ) -> Vec<ReviewedCommit> {
        let mut reviewed = Vec::new();
        for commit in commits {
            let change = self.head_change(repo, base, &commit, head).await;
            reviewed.push(ReviewedCommit { commit, change });
        }
        reviewed
    }

    async fn head_change(&self, repo: &str, base: &str, reviewed: &str, head: &str) -> HeadChange {
        // A retargeted or updated base changes the PR's changes at both commits.
        let key = (
            repo.to_owned(),
            base.to_owned(),
            reviewed.to_owned(),
            head.to_owned(),
        );
        let Ok(session) = self.session().await else {
            return HeadChange::Unavailable;
        };
        if let Some(change) = session.api.lock().unwrap().head_change(&key) {
            return change;
        }
        let change = match self.compare_head(repo, base, reviewed, head).await {
            Ok(change) => change,
            Err(error) => {
                tracing::warn!(event = "head_change_unavailable", repo, reviewed, head, error = %error);
                HeadChange::Unavailable
            }
        };
        tracing::info!(event = "head_change", repo, reviewed, head, change = ?change);
        session
            .api
            .lock()
            .unwrap()
            .remember_head_change(key, change);
        change
    }

    async fn compare_head(
        &self,
        repo: &str,
        base: &str,
        reviewed: &str,
        head: &str,
    ) -> Result<HeadChange> {
        ensure!(!base.is_empty(), "Missing PR base commit");
        // Each side is compared with its own merge base with the current base,
        // which is the PR's change at that commit.
        let before = self.compare(repo, base, reviewed).await?;
        let after = self.compare(repo, base, head).await?;
        if changes(&before)? == changes(&after)? {
            return Ok(HeadChange::Equivalent);
        }
        let between = self.compare(repo, reviewed, head).await?;
        Ok(if between["status"] == "ahead" {
            HeadChange::NewCommits
        } else {
            HeadChange::Rewritten
        })
    }

    async fn compare(&self, repo: &str, from: &str, to: &str) -> Result<Value> {
        // Files are listed on the first page; one commit keeps the response small.
        let endpoint = format!("repos/{repo}/compare/{from}...{to}?per_page=1");
        self.execute(&["api", "--hostname", "github.com", &endpoint], None)
            .await
    }
}

/// The added and removed lines of each changed file, ignoring context lines
/// and hunk positions. Files without a textual patch (binary or very large)
/// compare by their resulting blob.
fn changes(comparison: &Value) -> Result<Vec<(String, Vec<String>)>> {
    let files = comparison["files"]
        .as_array()
        .context("Missing compared files")?;
    ensure!(
        files.len() < MAX_FILES,
        "Comparison lists too many files to verify"
    );
    let mut changes = files
        .iter()
        .map(|file| {
            let path = format!(
                "{}\0{}",
                string(file, "previous_filename"),
                required(file, "filename")?
            );
            let lines = match file["patch"].as_str() {
                Some(patch) => patch
                    .lines()
                    .filter(|line| line.starts_with('+') || line.starts_with('-'))
                    .map(str::to_owned)
                    .collect(),
                None => vec![format!(
                    "{} {}",
                    required(file, "status")?,
                    required(file, "sha")?
                )],
            };
            Ok((path, lines))
        })
        .collect::<Result<Vec<_>>>()?;
    changes.sort();
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comparison(patch: &str) -> Value {
        json!({"files":[{"filename":"src/lib.rs","status":"modified","sha":"blob","patch":patch}]})
    }

    #[test]
    fn rebases_compare_only_added_and_removed_lines() {
        let reviewed = comparison("@@ -1,3 +1,3 @@ fn a()\n context\n-old\n+new\n tail");
        let rebased = comparison("@@ -40,3 +42,3 @@ fn b()\n moved context\n-old\n+new\n other");
        assert_eq!(changes(&reviewed).unwrap(), changes(&rebased).unwrap());
        let changed = comparison("@@ -1,3 +1,3 @@\n context\n-old\n+newer\n tail");
        assert_ne!(changes(&reviewed).unwrap(), changes(&changed).unwrap());
    }

    #[test]
    fn files_without_patches_compare_by_blob_and_large_comparisons_fail() {
        let binary =
            |sha: &str| json!({"files":[{"filename":"logo.png","status":"modified","sha":sha}]});
        assert_eq!(
            changes(&binary("a")).unwrap(),
            changes(&binary("a")).unwrap()
        );
        assert_ne!(
            changes(&binary("a")).unwrap(),
            changes(&binary("b")).unwrap()
        );
        let many: Vec<_> = (0..MAX_FILES)
            .map(|i| json!({"filename":format!("f{i}"),"status":"added","sha":"x","patch":"+x"}))
            .collect();
        assert!(changes(&json!({"files":many})).is_err());
        assert!(changes(&json!({})).is_err());
    }
}
