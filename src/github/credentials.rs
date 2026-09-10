//! Resolve active credentials locally, before applying any remote quota. Tokens
//! stay in memory and the child environment; only their hashes key shared state.
use super::*;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub(super) struct Session {
    pub token: String,
    pub api: Arc<Mutex<cache::ApiState>>,
}
impl Github {
    pub(super) async fn refresh_credentials(&self) -> Result<Session> {
        let mut command = Command::new(&self.executable);
        command
            .args(["auth", "token", "--hostname", "github.com"])
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_HOST", "github.com")
            .env_remove("GH_DEBUG")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(self.timeout, command.output())
            .await
            .context("Local GitHub credential lookup timed out")??;
        ensure!(
            output.status.success(),
            "Cannot read GitHub credentials; run gh auth login"
        );
        let token =
            String::from_utf8(output.stdout).context("Invalid GitHub credential encoding")?;
        let token = token.trim().to_owned();
        ensure!(
            !token.is_empty() && token.len() <= 16384 && !token.chars().any(char::is_whitespace),
            "Invalid GitHub credential; run gh auth login"
        );
        let session = Session {
            api: cache::credential(&self.executable, &hash(&token)),
            token,
        };
        *self.session.lock().unwrap() = Some(session.clone());
        Ok(session)
    }
    pub(super) async fn session(&self) -> Result<Session> {
        let session = self.session.lock().unwrap().clone();
        match session {
            Some(session) => Ok(session),
            None => self.refresh_credentials().await,
        }
    }
    pub(crate) async fn probe_credentials(config: &Config) -> Result<bool> {
        let github = Self::new(config)?;
        let previous = cache::active(&github.executable);
        let current = github.refresh_credentials().await?;
        Ok(previous.is_none_or(|previous| !Arc::ptr_eq(&previous, &current.api)))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn cooldown_probe_detects_account_switch_locally_without_resetting_old_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gh");
        std::fs::write(&path, "#!/bin/sh\nif [ \"$1\" = auth ]; then cat \"$(dirname \"$0\")/account\"; else exit 99; fi\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = Config {
            gh_path: Some(path),
            ..Default::default()
        };
        std::fs::write(dir.path().join("account"), "alpha").unwrap();
        let github = Github::new(&config).unwrap();
        let session = github.refresh_credentials().await.unwrap();
        session.api.lock().unwrap().observe(
            &BTreeMap::from([
                ("x-ratelimit-resource".into(), "graphql".into()),
                ("x-ratelimit-remaining".into(), "0".into()),
                (
                    "x-ratelimit-reset".into(),
                    (chrono::Utc::now().timestamp() + 1800).to_string(),
                ),
            ]),
            "graphql",
            true,
        );
        assert!(!Github::probe_credentials(&config).await.unwrap());
        assert!(Github::cooldown(&config, None).as_secs() > 1700);
        std::fs::write(dir.path().join("account"), "beta").unwrap();
        assert!(Github::probe_credentials(&config).await.unwrap());
        assert!(Github::cooldown(&config, None).is_zero());
        std::fs::write(dir.path().join("account"), "alpha").unwrap();
        assert!(Github::probe_credentials(&config).await.unwrap());
        assert!(Github::cooldown(&config, None).as_secs() > 1700);
    }
}
