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
        anyhow::ensure!(version <= 1, "Database belongs to a newer Gopher version");
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
            CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS prs (id TEXT PRIMARY KEY, data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS notifications (id TEXT PRIMARY KEY, pr_id TEXT NOT NULL, update_id TEXT NOT NULL, url TEXT NOT NULL, delivered INTEGER NOT NULL DEFAULT 0);
            PRAGMA user_version=1;")?;
        Ok(Self { connection })
    }

    pub fn set_viewer(&mut self, viewer: &str) -> Result<bool> {
        let previous: Option<String> = self
            .connection
            .query_row("SELECT value FROM metadata WHERE key='viewer'", [], |r| {
                r.get(0)
            })
            .optional()?;
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
