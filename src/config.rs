use crate::model::Agent;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub poll_seconds: u64,
    pub discovery_seconds: u64,
    pub request_timeout_seconds: u64,
    pub settle_seconds: u64,
    pub gh_path: Option<PathBuf>,
    pub log_level: String,
    pub notifications: bool,
    pub repositories: BTreeMap<String, RepositoryConfig>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepositoryConfig {
    /// When present, this is the complete participating reviewer set for this repo.
    pub reviewers: Option<Vec<Agent>>,
    pub ignore: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            poll_seconds: 30,
            discovery_seconds: 120,
            request_timeout_seconds: 20,
            settle_seconds: 30,
            gh_path: None,
            log_level: "info".into(),
            notifications: true,
            repositories: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn directory() -> Result<PathBuf> {
        Ok(
            PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
                .join(".config/gopher"),
        )
    }

    pub fn load(directory: &std::path::Path) -> Result<Self> {
        let path = directory.join("config.toml");
        let config: Self = match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("Invalid {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e.into()),
        };
        ensure!(config.poll_seconds >= 5, "poll_seconds must be at least 5");
        ensure!(
            config.discovery_seconds >= config.poll_seconds,
            "discovery_seconds must be >= poll_seconds"
        );
        ensure!(
            (1..=120).contains(&config.request_timeout_seconds),
            "request_timeout_seconds must be 1–120"
        );
        ensure!(
            config.settle_seconds <= 600,
            "settle_seconds must be <= 600"
        );
        tracing_subscriber::EnvFilter::try_new(&config.log_level).context("Invalid log_level")?;
        Ok(config)
    }
}
