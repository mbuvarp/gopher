use super::*;

impl Coordinator {
    pub fn label_saved(&mut self, pr: &str, name: &str) {
        self.completed_labels.remove(&(pr.into(), name.into()));
    }

    pub(super) fn sync_labels(&mut self, context: &Context<'_>) {
        let before = self.state.labels.clone();
        self.completed_labels
            .retain(|(id, _), _| context.prs.contains_key(id));
        self.state
            .labels
            .retain(|id, _| context.prs.contains_key(id));
        for (id, pr) in context.prs {
            let repo = repo_key(&pr.snapshot.repo);
            let catalogue = self.catalogues.get(&repo);
            let labels = self.state.labels.entry(id.clone()).or_default();
            let mut items: BTreeMap<String, Label> = catalogue
                .map(|c| {
                    c.labels
                        .iter()
                        .map(|l| {
                            (
                                l.name.clone(),
                                Label {
                                    name: l.name.clone(),
                                    color: l.color.clone(),
                                    selected: false,
                                },
                            )
                        })
                        .collect()
                })
                .unwrap_or_else(|| {
                    labels
                        .items
                        .iter()
                        .map(|l| (l.name.clone(), l.clone()))
                        .collect()
                });
            // Applied labels also cover a recently created label not in the catalogue yet.
            for label in &pr.snapshot.labels {
                items.insert(
                    label.name.clone(),
                    Label {
                        name: label.name.clone(),
                        color: label.color.clone(),
                        selected: true,
                    },
                );
            }
            // Pending mutations survive catalogue and PR snapshot refreshes.
            for previous in &labels.items {
                if labels.pending.contains(&previous.name)
                    || self
                        .completed_labels
                        .contains_key(&(id.clone(), previous.name.clone()))
                {
                    items.insert(previous.name.clone(), previous.clone());
                }
            }
            for label in items.values_mut() {
                if !labels.pending.contains(&label.name) {
                    label.selected = self
                        .completed_labels
                        .get(&(id.clone(), label.name.clone()))
                        .copied()
                        .unwrap_or_else(|| pr.snapshot.labels.iter().any(|l| l.name == label.name));
                }
            }
            labels.items = items.into_values().collect();
            labels.items.sort_by_cached_key(|l| l.name.to_lowercase());
            labels.loading = self.loads.contains_key(&repo);
            labels.catalogue_ready = catalogue.is_some();
            labels.catalogue_error = self.load_errors.get(&repo).cloned();
        }
        if before != self.state.labels {
            self.publish(context);
        }
    }

    /// Scheduled and manual refreshes share the same repository load token. Opening
    /// a picker never calls this; its selections come from the regular PR snapshot.
    pub fn refresh_catalogues(&mut self, context: &Context<'_>, manual_repo: Option<&str>) {
        let Some(viewer) = context.viewer else {
            return;
        };
        if self.viewer.as_deref() != Some(viewer) {
            return;
        }
        let now = chrono::Utc::now().timestamp();
        let repos: std::collections::BTreeSet<_> = context
            .prs
            .values()
            .filter(|pr| pr.snapshot.open)
            .map(|pr| repo_key(&pr.snapshot.repo))
            .collect();
        let mut jobs = Vec::new();
        for repo in repos {
            let manual = manual_repo.is_some_and(|r| repo_key(r) == repo);
            if manual_repo.is_some() && !manual {
                continue;
            }
            if self.loads.contains_key(&repo) {
                continue;
            }
            if !manual
                && (self
                    .catalogues
                    .get(&repo)
                    .is_some_and(|c| (0..900).contains(&now.saturating_sub(c.fetched_at)))
                    || self
                        .attempts
                        .get(&repo)
                        .is_some_and(|t| t.elapsed() < Duration::from_secs(900)))
            {
                continue;
            }
            // Respect the transport cooldown without consuming this repo's refresh interval.
            if !Github::cooldown(context.config).is_zero() {
                if manual {
                    self.load_errors.insert(
                        repo.clone(),
                        "GitHub rate limit: label refresh is paused until the quota cooldown ends."
                            .into(),
                    );
                    self.sync_labels(context);
                }
                continue;
            }
            let token = self.token();
            self.loads.insert(repo.clone(), token);
            self.attempts
                .insert(repo.clone(), std::time::Instant::now());
            self.load_errors.remove(&repo);
            for pr in context
                .prs
                .values()
                .filter(|pr| repo_key(&pr.snapshot.repo) == repo)
            {
                if let Some(labels) = self.state.labels.get_mut(&pr.snapshot.id) {
                    labels.catalogue_error = None;
                }
            }
            jobs.push((repo, token));
        }
        if jobs.is_empty() {
            return;
        }
        self.sync_labels(context);
        let config = context.config.clone();
        let sender = context.sender.clone();
        let viewer = viewer.to_owned();
        tokio::spawn(async move {
            // Authenticate once for the entire scheduled batch of repositories.
            let github = async {
                let github = Github::new(&config)?;
                ensure!(github.viewer().await? == viewer, "GitHub account changed");
                Ok::<_, anyhow::Error>(github)
            }
            .await
            .map_err(|e| e.to_string());
            let mut results = Vec::new();
            for (repo, token) in jobs {
                let result = match &github {
                    Ok(github) => github
                        .repository_labels(&repo)
                        .await
                        .map_err(|e| e.to_string()),
                    Err(error) => Err(error.clone()),
                };
                results.push((repo, token, result));
            }
            let verified = match &github {
                Ok(github) => {
                    github
                        .viewer()
                        .await
                        .map_err(|e| e.to_string())
                        .and_then(|account| {
                            if account == viewer {
                                Ok(())
                            } else {
                                Err("GitHub account changed".into())
                            }
                        })
                }
                Err(error) => Err(error.clone()),
            };
            for (repo, token, result) in results {
                let result = verified.clone().and(result);
                let _ = sender.send(Command::PrAction(ActionCommand::LabelsLoaded {
                    repo,
                    token,
                    result,
                }));
            }
        });
    }
}
