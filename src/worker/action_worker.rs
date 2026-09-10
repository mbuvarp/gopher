//! Mutations are explicitly requested, checked in the worker, and never retried
//! automatically. Pending merges are memory-only and disappear on shutdown.
use super::{Command, Sink, UiEvent, transition};
use crate::{
    actions::*,
    config::Config,
    github::{Github, PrRef},
    model::{PullRequest, Snapshot},
    store::Store,
};
use anyhow::{Context as _, Result, ensure};
use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};
use tokio::sync::mpsc::UnboundedSender;

#[cfg(all(test, unix))]
mod tests;

pub enum ActionCommand {
    Request(Request),
    Tick {
        pr: String,
        token: u64,
        remaining: u8,
    },
    MergeChecked {
        pr: String,
        token: u64,
        result: Result<Box<Snapshot>, String>,
    },
    Merged {
        pr: String,
        token: u64,
        result: Result<(), String>,
    },
    LabelsLoaded {
        pr: String,
        token: u64,
        result: Result<Vec<Label>, String>,
    },
    LabelChecked {
        pr: String,
        token: u64,
        result: Result<(), String>,
    },
    LabelDone {
        pr: String,
        token: u64,
        result: Result<(), String>,
    },
}

pub(super) struct Context<'a> {
    pub store: &'a Store,
    pub prs: &'a BTreeMap<String, PullRequest>,
    pub viewer: Option<&'a str>,
    pub config: &'a Config,
    pub sender: &'a UnboundedSender<Command>,
    pub sink: &'a Sink,
}

#[derive(Clone)]
struct Intent {
    token: u64,
    pr: PullRequest,
    viewer: String,
    preferences: Preferences,
}
impl Intent {
    fn reference(&self) -> PrRef {
        PrRef {
            id: self.pr.snapshot.id.clone(),
            repo: self.pr.snapshot.repo.clone(),
            number: self.pr.snapshot.number,
        }
    }
    fn valid(&self, context: &Context<'_>, state: &ActionState, kind: Kind) -> bool {
        context.viewer == Some(self.viewer.as_str())
            && context.prs.get(&self.pr.snapshot.id).is_some_and(|pr| {
                let preferences = state.preferences(&pr.snapshot.repo);
                preferences.rule(kind) == self.preferences.rule(kind)
                    && (kind != Kind::Merge
                        || preferences.merge_method == self.preferences.merge_method)
                    && preferences.allows(kind, pr)
                    && (kind != Kind::Merge || pr.snapshot.head == self.pr.snapshot.head)
            })
    }
}
#[derive(Clone)]
struct LabelJob {
    intent: Intent,
    name: String,
    selected: bool,
    previous: bool,
}

pub(super) struct Coordinator {
    pub state: ActionState,
    viewer: Option<String>,
    sequence: u64,
    merges: BTreeMap<String, Intent>,
    loads: BTreeMap<String, u64>,
    label_jobs: BTreeMap<String, VecDeque<LabelJob>>,
}
impl Coordinator {
    pub fn new(store: &Store) -> Result<Self> {
        Ok(Self {
            state: ActionState {
                preferences: store.action_preferences()?,
                ..Default::default()
            },
            viewer: store.viewer()?,
            sequence: 0,
            merges: BTreeMap::new(),
            loads: BTreeMap::new(),
            label_jobs: BTreeMap::new(),
        })
    }
    fn token(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence
    }
    pub fn publish(&self, context: &Context<'_>) {
        (context.sink)(UiEvent::ActionsChanged(self.state.clone()));
    }
    fn intent(&mut self, id: &str, kind: Kind, context: &Context<'_>) -> Result<Intent> {
        let pr = context
            .prs
            .get(id)
            .context("This PR is no longer in the active list")?;
        let preferences = self.state.preferences(&pr.snapshot.repo);
        ensure!(
            preferences.allows(kind, pr),
            "Action is unavailable for this PR's current state"
        );
        Ok(Intent {
            token: self.token(),
            pr: pr.clone(),
            viewer: context
                .viewer
                .context("Waiting for GitHub authentication")?
                .into(),
            preferences,
        })
    }
    pub fn reconcile(&mut self, context: &Context<'_>) {
        if self.viewer.as_deref() != context.viewer {
            let changed = !self.state.merges.is_empty() || !self.state.labels.is_empty();
            self.viewer = context.viewer.map(str::to_owned);
            self.merges.clear();
            self.loads.clear();
            self.label_jobs.clear();
            self.state.merges.clear();
            self.state.labels.clear();
            if changed {
                self.publish(context);
            }
            return;
        }
        let cancelled = self
            .merges
            .iter()
            .filter(|(id, intent)| {
                self.state.merges.get(*id) != Some(&MergeProgress::Merging)
                    && !intent.valid(context, &self.state, Kind::Merge)
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        if !cancelled.is_empty() {
            for id in cancelled {
                self.merges.remove(&id);
                self.state.merges.insert(
                    id,
                    MergeProgress::Failed(
                        "Merge cancelled: the PR or action settings changed.".into(),
                    ),
                );
            }
            self.publish(context);
        }
    }
    pub fn handle(&mut self, command: ActionCommand, context: &Context<'_>) {
        match command {
            ActionCommand::Request(request) => {
                if let Err(error) = self.request(request, context) {
                    tracing::warn!(event="pr_action_rejected", error=%error);
                    self.state.error = Some(error.to_string());
                }
            }
            ActionCommand::Tick {
                pr,
                token,
                remaining,
            } => {
                if self.merges.get(&pr).is_some_and(|i| i.token == token) {
                    if remaining > 0 {
                        self.state
                            .merges
                            .insert(pr, MergeProgress::Countdown(remaining));
                    } else {
                        self.check_merge(&pr, context);
                    }
                }
            }
            ActionCommand::MergeChecked { pr, token, result } => {
                if let Some(intent) = self.merges.get(&pr).filter(|i| i.token == token).cloned() {
                    let checked = result.and_then(|snapshot| {
                        let expected = context.config.repositories.get(&snapshot.repo).and_then(|r| r.reviewers.as_deref());
                        let fresh = transition(*snapshot, Some(&intent.pr), expected, chrono::Utc::now().timestamp(), context.config.settle_seconds);
                        if intent.valid(context, &self.state, Kind::Merge) && intent.preferences.allows(Kind::Merge, &fresh) && fresh.snapshot.head == intent.pr.snapshot.head {
                            Ok(())
                        } else { Err("Merge cancelled: the commit, review state, or action settings changed.".into()) }
                    });
                    match checked {
                        Err(error) => {
                            self.merges.remove(&pr);
                            self.state.merges.insert(pr, MergeProgress::Failed(error));
                        }
                        Ok(()) => {
                            self.state.merges.insert(pr.clone(), MergeProgress::Merging);
                            let config = context.config.clone();
                            let sender = context.sender.clone();
                            tokio::spawn(async move {
                                let result = async {
                                    let github = Github::new(&config)?;
                                    // All read requests finish before MergeChecked.
                                    // The worker's final validation authorizes submission;
                                    // do not add another await before the merge request.
                                    github
                                        .merge_pr(
                                            &intent.reference(),
                                            &intent.pr.snapshot.head,
                                            intent.preferences.merge_method,
                                        )
                                        .await
                                }
                                .await
                                .map_err(|e: anyhow::Error| e.to_string());
                                let _ = sender.send(Command::PrAction(ActionCommand::Merged {
                                    pr,
                                    token,
                                    result,
                                }));
                            });
                        }
                    }
                }
            }
            ActionCommand::Merged { pr, token, result } => {
                if self.merges.get(&pr).is_some_and(|i| i.token == token) {
                    self.merges.remove(&pr);
                    match result {
                        Ok(()) => {
                            tracing::info!(event="pr_merged", pr_id=%pr);
                            self.state.merges.insert(pr, MergeProgress::Complete);
                            let _ = context.sender.send(Command::Refresh);
                        }
                        Err(error) => {
                            tracing::warn!(event="merge_failed", pr_id=%pr, error=%error);
                            self.state.merges.insert(pr, MergeProgress::Failed(error));
                        }
                    }
                }
            }
            ActionCommand::LabelsLoaded { pr, token, result } => {
                if self.loads.get(&pr) == Some(&token) {
                    self.loads.remove(&pr);
                    let labels = self.state.labels.entry(pr).or_default();
                    labels.loading = false;
                    match result {
                        Ok(items) => {
                            labels.items = items;
                            labels.error = None;
                        }
                        Err(error) => {
                            tracing::warn!(event="labels_load_failed", error=%error);
                            labels.error = Some(error);
                        }
                    }
                }
            }
            ActionCommand::LabelChecked { pr, token, result } => {
                if let Some(job) = self
                    .label_jobs
                    .get(&pr)
                    .and_then(|jobs| jobs.front())
                    .filter(|j| j.intent.token == token)
                    .cloned()
                {
                    match result {
                        Err(error) => self.finish_label(&pr, token, Err(error), context),
                        Ok(()) if !job.intent.valid(context, &self.state, Kind::Label) => self
                            .finish_label(
                                &pr,
                                token,
                                Err("Label action cancelled: PR or settings changed.".into()),
                                context,
                            ),
                        Ok(()) => {
                            let config = context.config.clone();
                            let sender = context.sender.clone();
                            tokio::spawn(async move {
                                let result = async {
                                    let github = Github::new(&config)?;
                                    ensure!(
                                        github.viewer().await? == job.intent.viewer,
                                        "GitHub account changed; label action cancelled"
                                    );
                                    github
                                        .set_label(&job.intent.reference(), &job.name, job.selected)
                                        .await
                                }
                                .await
                                .map_err(|e: anyhow::Error| e.to_string());
                                let _ = sender.send(Command::PrAction(ActionCommand::LabelDone {
                                    pr,
                                    token,
                                    result,
                                }));
                            });
                        }
                    }
                }
            }
            ActionCommand::LabelDone { pr, token, result } => {
                self.finish_label(&pr, token, result, context)
            }
        }
        self.reconcile(context);
        self.publish(context);
    }

    fn request(&mut self, request: Request, context: &Context<'_>) -> Result<()> {
        self.state.error = None;
        match request {
            Request::Configure { repo, change } => {
                let mut preferences = self.state.preferences(&repo);
                preferences.apply(&change);
                context.store.save_action_preferences(&repo, &preferences)?;
                self.state.preferences.insert(repo_key(&repo), preferences);
                tracing::info!(event="action_settings_saved", repo=%repo);
            }
            Request::Merge { pr, head } => {
                ensure!(
                    !self.merges.contains_key(&pr),
                    "A merge is already pending for this PR"
                );
                let intent = self.intent(&pr, Kind::Merge, context)?;
                ensure!(
                    head == intent.pr.snapshot.head && !head.is_empty(),
                    "The PR commit changed; refresh before merging"
                );
                let token = intent.token;
                self.merges.insert(pr.clone(), intent);
                self.state
                    .merges
                    .insert(pr.clone(), MergeProgress::Countdown(5));
                tracing::info!(event="merge_countdown_started", pr_id=%pr);
                let sender = context.sender.clone();
                tokio::spawn(async move {
                    let start = tokio::time::Instant::now();
                    for elapsed in 1..=5 {
                        tokio::time::sleep_until(start + Duration::from_secs(elapsed)).await;
                        let _ = sender.send(Command::PrAction(ActionCommand::Tick {
                            pr: pr.clone(),
                            token,
                            remaining: (5 - elapsed) as u8,
                        }));
                    }
                });
            }
            Request::CancelMerge(pr) => {
                if self.state.merges.get(&pr) != Some(&MergeProgress::Merging) {
                    self.merges.remove(&pr);
                    self.state.merges.remove(&pr);
                    tracing::info!(event="merge_cancelled", pr_id=%pr);
                }
            }
            Request::LoadLabels(pr) => {
                let intent = self.intent(&pr, Kind::Label, context)?;
                if self.loads.contains_key(&pr)
                    || self
                        .label_jobs
                        .get(&pr)
                        .is_some_and(|jobs| !jobs.is_empty())
                {
                    return Ok(());
                }
                let token = intent.token;
                self.loads.insert(pr.clone(), token);
                let labels = self.state.labels.entry(pr.clone()).or_default();
                labels.loading = true;
                labels.error = None;
                let config = context.config.clone();
                let sender = context.sender.clone();
                tokio::spawn(async move {
                    let result = async {
                        let github = Github::new(&config)?;
                        ensure!(
                            github.viewer().await? == intent.viewer,
                            "GitHub account changed"
                        );
                        github.labels(&intent.reference()).await
                    }
                    .await
                    .map_err(|e: anyhow::Error| e.to_string());
                    let _ = sender.send(Command::PrAction(ActionCommand::LabelsLoaded {
                        pr,
                        token,
                        result,
                    }));
                });
            }
            Request::Label { pr, name, selected } => {
                let intent = self.intent(&pr, Kind::Label, context)?;
                let labels = self
                    .state
                    .labels
                    .get_mut(&pr)
                    .context("Load the PR labels first")?;
                ensure!(
                    !labels.loading && !labels.pending.contains(&name),
                    "Label update already pending"
                );
                let label = labels
                    .items
                    .iter_mut()
                    .find(|label| label.name == name)
                    .context("Label no longer exists")?;
                let previous = label.selected;
                if previous == selected {
                    return Ok(());
                }
                label.selected = selected;
                labels.pending.insert(name.clone());
                labels.error = None;
                let jobs = self.label_jobs.entry(pr.clone()).or_default();
                jobs.push_back(LabelJob {
                    intent,
                    name,
                    selected,
                    previous,
                });
                if jobs.len() == 1 {
                    self.start_label(&pr, context);
                }
            }
        }
        Ok(())
    }

    fn check_merge(&mut self, pr: &str, context: &Context<'_>) {
        let Some(intent) = self.merges.get(pr).cloned() else {
            return;
        };
        self.state.merges.insert(pr.into(), MergeProgress::Checking);
        let config = context.config.clone();
        let sender = context.sender.clone();
        let pr = pr.to_owned();
        tokio::spawn(async move {
            let result = async {
                let github = Github::new(&config)?;
                ensure!(
                    github.viewer().await? == intent.viewer,
                    "GitHub account changed; merge cancelled"
                );
                let snapshot = github.snapshot(&intent.reference()).await?;
                // Remain cancellable while checking authentication, then let the
                // worker revalidate its latest PR state and settings before merging.
                ensure!(
                    github.viewer().await? == intent.viewer,
                    "GitHub account changed; merge cancelled"
                );
                Ok(Box::new(snapshot))
            }
            .await
            .map_err(|e: anyhow::Error| e.to_string());
            let _ = sender.send(Command::PrAction(ActionCommand::MergeChecked {
                pr,
                token: intent.token,
                result,
            }));
        });
    }
    fn start_label(&mut self, pr: &str, context: &Context<'_>) {
        let Some(job) = self
            .label_jobs
            .get(pr)
            .and_then(|jobs| jobs.front())
            .cloned()
        else {
            return;
        };
        let pr = pr.to_owned();
        let sender = context.sender.clone();
        let config = context.config.clone();
        if !job.intent.valid(context, &self.state, Kind::Label) {
            let _ = sender.send(Command::PrAction(ActionCommand::LabelChecked {
                pr,
                token: job.intent.token,
                result: Err("Label action cancelled: PR or settings changed.".into()),
            }));
            return;
        }
        tokio::spawn(async move {
            let result = async {
                ensure!(
                    Github::new(&config)?.viewer().await? == job.intent.viewer,
                    "GitHub account changed; label action cancelled"
                );
                Ok(())
            }
            .await
            .map_err(|e: anyhow::Error| e.to_string());
            let _ = sender.send(Command::PrAction(ActionCommand::LabelChecked {
                pr,
                token: job.intent.token,
                result,
            }));
        });
    }
    fn finish_label(
        &mut self,
        pr: &str,
        token: u64,
        result: Result<(), String>,
        context: &Context<'_>,
    ) {
        let Some(jobs) = self.label_jobs.get_mut(pr) else {
            return;
        };
        if !jobs.front().is_some_and(|j| j.intent.token == token) {
            return;
        }
        let job = jobs.pop_front().unwrap();
        if let Some(labels) = self.state.labels.get_mut(pr) {
            labels.pending.remove(&job.name);
            if let Err(error) = result {
                if let Some(label) = labels.items.iter_mut().find(|label| label.name == job.name) {
                    label.selected = job.previous;
                }
                tracing::warn!(event="label_update_failed", pr_id=%pr, error=%error);
                labels.error = Some(error);
            } else {
                tracing::info!(event="label_updated", pr_id=%pr, label=%job.name, selected=job.selected);
                if let Some(label) = labels.items.iter().find(|label| label.name == job.name) {
                    let _ = context.sender.send(Command::LabelSaved {
                        pr: pr.into(),
                        viewer: job.intent.viewer.clone(),
                        label: crate::model::PrLabel {
                            name: label.name.clone(),
                            color: label.color.clone(),
                        },
                        selected: job.selected,
                    });
                }
            }
        }
        self.start_label(pr, context);
    }
}
