use crate::model::*;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::{collections::BTreeSet, path::Path};

pub struct Store {
    connection: Connection,
}
impl Store {
    pub fn open(directory: &Path) -> Result<Self> {
        std::fs::create_dir_all(directory)?;
        let connection = Connection::open(directory.join("state.sqlite3"))?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        anyhow::ensure!(version <= 2, "Database belongs to a newer Gopher version");
        // The optional details table is additive; keep compatibility with the main-branch app.
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS prs (id TEXT PRIMARY KEY, data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS notifications (id TEXT PRIMARY KEY, pr_id TEXT NOT NULL, update_id TEXT NOT NULL, url TEXT NOT NULL, delivered INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS ignored_prs (id TEXT PRIMARY KEY);
            CREATE TABLE IF NOT EXISTS ignored_pr_details (id TEXT PRIMARY KEY, data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS repository_actions (repo TEXT PRIMARY KEY, data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS repository_labels (viewer TEXT NOT NULL, repo TEXT NOT NULL, data TEXT NOT NULL, PRIMARY KEY(viewer,repo));
            PRAGMA user_version=2; COMMIT;")?;
        Ok(Self { connection })
    }

    pub fn hotkeys(&self) -> Result<crate::hotkeys::Preferences> {
        let data: Option<String> = self
            .connection
            .query_row("SELECT value FROM metadata WHERE key='hotkeys'", [], |r| {
                r.get(0)
            })
            .optional()?;
        let preferences: crate::hotkeys::Preferences = data
            .map(|text| serde_json::from_str(&text))
            .transpose()?
            .unwrap_or_default();
        preferences.validate()?;
        Ok(preferences)
    }
    pub fn save_hotkeys(&self, preferences: &crate::hotkeys::Preferences) -> Result<()> {
        preferences.validate()?;
        self.connection.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES ('hotkeys',?1)",
            [serde_json::to_string(preferences)?],
        )?;
        tracing::info!(event = "hotkeys_saved");
        Ok(())
    }

    pub fn ignored(&self) -> Result<BTreeSet<String>> {
        let mut query = self.connection.prepare("SELECT id FROM ignored_prs")?;
        Ok(query
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn action_preferences(
        &self,
    ) -> Result<std::collections::BTreeMap<String, crate::actions::Preferences>> {
        let mut query = self
            .connection
            .prepare("SELECT repo,data FROM repository_actions")?;
        query
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (repo, data) = row?;
                Ok((
                    repo,
                    serde_json::from_str(&data).context("Invalid repository action settings")?,
                ))
            })
            .collect()
    }

    pub fn save_action_preferences(
        &self,
        repo: &str,
        preferences: &crate::actions::Preferences,
    ) -> Result<()> {
        self.connection.execute(
            "INSERT OR REPLACE INTO repository_actions (repo,data) VALUES (?1,?2)",
            params![
                crate::actions::repo_key(repo),
                serde_json::to_string(preferences)?
            ],
        )?;
        Ok(())
    }

    pub fn label_catalogues(
        &self,
        viewer: &str,
    ) -> Result<std::collections::BTreeMap<String, crate::actions::LabelCatalogue>> {
        let mut query = self
            .connection
            .prepare("SELECT repo,data FROM repository_labels WHERE viewer=?1")?;
        query
            .query_map([viewer], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (repo, data) = row?;
                Ok((
                    repo,
                    serde_json::from_str(&data).context("Invalid repository label cache")?,
                ))
            })
            .collect()
    }

    pub fn save_label_catalogue(
        &self,
        viewer: &str,
        repo: &str,
        catalogue: &crate::actions::LabelCatalogue,
    ) -> Result<()> {
        self.connection.execute(
            "INSERT OR REPLACE INTO repository_labels (viewer,repo,data) VALUES (?1,?2,?3)",
            params![
                viewer,
                crate::actions::repo_key(repo),
                serde_json::to_string(catalogue)?
            ],
        )?;
        Ok(())
    }

    pub fn ignore(&mut self, id: &str) -> Result<Vec<String>> {
        let notifications = self.notification_ids(id, None)?;
        let tx = self.connection.transaction()?;
        tx.execute("INSERT OR IGNORE INTO ignored_prs (id) VALUES (?1)", [id])?;
        tx.execute("INSERT OR REPLACE INTO ignored_pr_details (id,data) SELECT id,data FROM prs WHERE id=?1", [id])?;
        tx.execute("DELETE FROM prs WHERE id=?1", [id])?;
        tx.execute("DELETE FROM notifications WHERE pr_id=?1", [id])?;
        tx.commit()?;
        tracing::info!(event = "pr_ignored", pr_id = id);
        Ok(notifications)
    }

    pub fn load_ignored(&self) -> Result<Vec<PullRequest>> {
        let mut query = self.connection.prepare("SELECT i.id,d.data FROM ignored_prs i LEFT JOIN ignored_pr_details d ON d.id=i.id ORDER BY i.id")?;
        let entries: Vec<PullRequest> = query
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .map(|row| {
                let (id, data) = row?;
                let mut pr = match data {
                    Some(data) => {
                        serde_json::from_str(&data).context("Invalid ignored PR details")?
                    }
                    None => PullRequest::unreviewed(Snapshot {
                        id,
                        repo: "Unavailable".into(),
                        title: "Pull request details unavailable".into(),
                        ..Default::default()
                    }),
                };
                pr.stale = true;
                Ok(pr)
            })
            .collect::<Result<_>>()?;
        // Unknown/inaccessible identities remain visible; only confirmed closures hide.
        Ok(entries
            .into_iter()
            .filter(|pr| pr.snapshot.number == 0 || pr.snapshot.open)
            .collect())
    }

    pub fn update_ignored_status(&self, snapshot: &Snapshot) -> Result<()> {
        let data: Option<String> = self
            .connection
            .query_row(
                "SELECT data FROM ignored_pr_details WHERE id=?1",
                [&snapshot.id],
                |row| row.get(0),
            )
            .optional()?;
        let mut pr = match data {
            Some(data) => {
                serde_json::from_str::<PullRequest>(&data).context("Invalid ignored PR details")?
            }
            None => PullRequest::unreviewed(snapshot.clone()),
        };
        // Refresh identity and lifecycle without overwriting cached review evidence.
        pr.snapshot.repo.clone_from(&snapshot.repo);
        pr.snapshot.number = snapshot.number;
        pr.snapshot.title.clone_from(&snapshot.title);
        pr.snapshot.url.clone_from(&snapshot.url);
        pr.snapshot.open = snapshot.open;
        pr.snapshot.draft = snapshot.draft;
        pr.stale = true;
        self.connection.execute("INSERT OR REPLACE INTO ignored_pr_details (id,data) SELECT id,?2 FROM ignored_prs WHERE id=?1", params![snapshot.id, serde_json::to_string(&pr)?])?;
        Ok(())
    }

    pub fn save_ignored_details(&self, pr: &PullRequest) -> Result<()> {
        // A lookup that finishes after Restore must not recreate an ignored entry.
        self.connection.execute("INSERT OR IGNORE INTO ignored_pr_details (id,data) SELECT id,?2 FROM ignored_prs WHERE id=?1", params![pr.snapshot.id, serde_json::to_string(pr)?])?;
        Ok(())
    }

    pub fn restore(&mut self, id: &str) -> Result<bool> {
        let tx = self.connection.transaction()?;
        let removed = tx.execute("DELETE FROM ignored_prs WHERE id=?1", [id])? > 0;
        tx.execute("DELETE FROM ignored_pr_details WHERE id=?1", [id])?;
        tx.commit()?;
        if removed {
            tracing::info!(event = "pr_restored", pr_id = id);
        }
        Ok(removed)
    }

    pub fn notification_ids(&self, pr: &str, update: Option<&str>) -> Result<Vec<String>> {
        let mut query = self.connection.prepare(
            "SELECT id FROM notifications WHERE pr_id=?1 AND (?2 IS NULL OR update_id=?2)",
        )?;
        Ok(query
            .query_map(params![pr, update], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn viewer(&self) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row("SELECT value FROM metadata WHERE key='viewer'", [], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_viewer(&mut self, viewer: &str) -> Result<bool> {
        let previous = self.viewer()?;
        let tx = self.connection.transaction()?;
        let changed = previous.as_deref().is_some_and(|old| old != viewer);
        if changed {
            tx.execute("DELETE FROM prs", [])?;
            tx.execute("DELETE FROM notifications", [])?;
            tracing::info!(
                event = "account_changed",
                "Cleared cached state for the previous GitHub account"
            );
        }
        tx.execute(
            "INSERT OR REPLACE INTO metadata (key,value) VALUES ('viewer',?1)",
            [viewer],
        )?;
        tx.commit()?;
        Ok(changed)
    }

    pub fn load(&self) -> Result<Vec<PullRequest>> {
        let mut statement = self
            .connection
            .prepare("SELECT data FROM prs ORDER BY id")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| {
            let mut pr: PullRequest =
                serde_json::from_str(&row?).context("Invalid cached PR state")?;
            pr.stale = true;
            Ok(pr)
        })
        .collect()
    }

    pub fn save(&self, pr: &PullRequest) -> Result<()> {
        self.connection.execute(
            "INSERT OR REPLACE INTO prs (id,data) VALUES (?1,?2)",
            params![pr.snapshot.id, serde_json::to_string(pr)?],
        )?;
        Ok(())
    }

    pub fn retain(&mut self, ids: &BTreeSet<String>) -> Result<()> {
        let tx = self.connection.transaction()?;
        let all = {
            let mut q = tx.prepare("SELECT id FROM prs")?;
            q.query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for id in all.into_iter().filter(|id| !ids.contains(id)) {
            tx.execute("DELETE FROM prs WHERE id=?1", [&id])?;
            // Delivered notification clicks for closed PRs may still open their URLs.
        }
        tx.execute("DELETE FROM notifications WHERE rowid NOT IN (SELECT rowid FROM notifications ORDER BY rowid DESC LIMIT 10000)", [])?;
        tx.commit()?;
        Ok(())
    }

    pub fn acknowledge(&self, pr: &mut PullRequest, update: &str, checked: bool) -> Result<bool> {
        if pr.update_id != update {
            return Ok(false);
        }
        pr.acknowledged = checked.then(|| update.to_owned());
        self.save(pr)?;
        tracing::info!(event="acknowledgement", repo=%pr.snapshot.repo, pr=pr.snapshot.number, checked);
        Ok(true)
    }

    pub fn notification(&self, pr: &PullRequest) -> Result<Option<String>> {
        let id = hash(format!("{}:{}", pr.snapshot.id, pr.update_id));
        let delivered: Option<bool> = self
            .connection
            .query_row(
                "SELECT delivered FROM notifications WHERE id=?1",
                [&id],
                |r| r.get(0),
            )
            .optional()?;
        if delivered == Some(true) {
            return Ok(None);
        }
        self.connection.execute(
            "INSERT OR IGNORE INTO notifications (id,pr_id,update_id,url) VALUES (?1,?2,?3,?4)",
            params![id, pr.snapshot.id, pr.update_id, pr.snapshot.url],
        )?;
        Ok(Some(id))
    }
    pub fn mark_delivered(&self, id: &str) -> Result<()> {
        self.connection
            .execute("UPDATE notifications SET delivered=1 WHERE id=?1", [id])?;
        Ok(())
    }
    pub fn notification_target(&self, id: &str) -> Result<Option<(String, String, String)>> {
        Ok(self
            .connection
            .query_row(
                "SELECT pr_id,update_id,url FROM notifications WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }
}
