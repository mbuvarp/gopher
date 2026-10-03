//! Mutations are explicitly requested, checked in the worker, and never retried
//! automatically. Pending merges are memory-only and disappear on shutdown.
use super::{Command, Sink, UiEvent, transition_with_grace};
use crate::{
    actions::*,
    config::Config,
    github::{Github, PrRef},
    model::{CheckState, PullRequest, Snapshot},
    store::Store,
};
use anyhow::{Context as _, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::Duration,
};
use tokio::sync::mpsc::UnboundedSender;

#[cfg(all(test, unix))]
mod tests;

/// Read-only wait for GitHub to recalculate mergeability, bounded by wall-clock
/// time so slow requests cannot hold a base branch's merge queue.
#[cfg(not(test))]
const MERGEABILITY_WAIT: Duration = Duration::from_secs(30);
#[cfg(not(test))]
const MERGEABILITY_POLL: Duration = Duration::from_secs(2);
#[cfg(test)]
const MERGEABILITY_WAIT: Duration = Duration::from_millis(300);
#[cfg(test)]
const MERGEABILITY_POLL: Duration = Duration::from_millis(20);

mod catalogues;

pub enum ActionCommand {
    Request(Request),
    ReadyChecked {
        pr: String,
        token: u64,
        result: Result<(Box<Snapshot>, Github), String>,
    },
    ReadyDone {
        pr: String,
        token: u64,
        result: Result<(), String>,
    },
    Tick {
        pr: String,
        token: u64,
        remaining: u8,
    },
    MergeChecked {
        pr: String,
        token: u64,
        result: Result<(Box<Snapshot>, Github), String>,
    },
    Merged {
        pr: String,
        token: u64,
        result: Result<(), String>,
    },
    LabelsLoaded {
        repo: String,
        token: u64,
        result: Result<Vec<crate::model::PrLabel>, String>,
    },
    LabelChecked {
        pr: String,
        token: u64,
        result: Result<Github, String>,
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
    on_green: bool,
}
impl Intent {
    fn reference(&self) -> PrRef {
        PrRef {
            id: self.pr.snapshot.id.clone(),
            repo: self.pr.snapshot.repo.clone(),
            number: self.pr.snapshot.number,
        }
    }
    /// The merge lane from the latest PR state, so a retargeted PR cannot be
    /// treated as independent of merges into its new base branch.
    fn lane(&self, context: &Context<'_>) -> (String, Option<String>) {
        let snapshot = context
            .prs
            .get(&self.pr.snapshot.id)
            .map_or(&self.pr.snapshot, |pr| &pr.snapshot);
        (
            repo_key(&snapshot.repo),
            snapshot
                .base_branch
                .clone()
                .or_else(|| self.pr.snapshot.base_branch.clone()),
        )
    }
    fn valid(&self, context: &Context<'_>, state: &ActionState, kind: Kind) -> bool {
        context.viewer == Some(self.viewer.as_str())
            && context.prs.get(&self.pr.snapshot.id).is_some_and(|pr| {
                let preferences = state.preferences(&pr.snapshot.repo);
                preferences.rule(kind) == self.preferences.rule(kind)
                    && (kind != Kind::Merge
                        || preferences.merge_method == self.preferences.merge_method)
                    && preferences.allows(kind, pr)
                    && (kind != Kind::Merge
                        || (pr.snapshot.head == self.pr.snapshot.head
                            && !retargeted(&self.pr.snapshot, &pr.snapshot)
                            && pr.update_id == self.pr.update_id
                            && (!self.on_green
                                || matches!(
                                    pr.snapshot.check_state,
                                    Some(CheckState::Running | CheckState::Green)
                                ))))
            })
    }
}
/// An unknown base branch conservatively shares a lane with the whole repo.
fn shares_lane(a: &(String, Option<String>), b: &(String, Option<String>)) -> bool {
    a.0 == b.0 && (a.1.is_none() || b.1.is_none() || a.1 == b.1)
}
/// Merging into a different base than the one displayed changes the action.
fn retargeted(displayed: &Snapshot, current: &Snapshot) -> bool {
    matches!(
        (&displayed.base_branch, &current.base_branch),
        (Some(displayed), Some(current)) if displayed != current
    )
}
#[derive(Clone)]
struct LabelJob {
    intent: Intent,
    name: String,
    selected: bool,
    color: String,
}
#[derive(Clone)]
struct ReadyIntent {
    token: u64,
    reference: PrRef,
    viewer: String,
    head: String,
    update: String,
}

pub(super) struct Coordinator {
    pub state: ActionState,
    viewer: Option<String>,
    sequence: u64,
    merges: BTreeMap<String, Intent>,
    ready: BTreeMap<String, ReadyIntent>,
    loads: BTreeMap<String, u64>,
    catalogues: BTreeMap<String, LabelCatalogue>,
    attempts: BTreeMap<String, std::time::Instant>,
    load_errors: BTreeMap<String, String>,
    completed_labels: BTreeMap<(String, String), bool>,
    label_jobs: BTreeMap<String, VecDeque<LabelJob>>,
    shutting_down: bool,
    // Independent of UI/account state: a submitted request must finish even if
    // its intent is later invalidated by a poll or account change.
    submissions: BTreeSet<u64>,
    submitted_ready: BTreeMap<u64, ReadyIntent>,
    submitted_labels: BTreeMap<u64, LabelJob>,
    /// PRs GitHub confirmed merged, awaiting the worker's merged display window.
    merged: Vec<String>,
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
            ready: BTreeMap::new(),
            loads: BTreeMap::new(),
            catalogues: store.label_catalogues(store.viewer()?.as_deref().unwrap_or(""))?,
            attempts: BTreeMap::new(),
            load_errors: BTreeMap::new(),
            completed_labels: BTreeMap::new(),
            label_jobs: BTreeMap::new(),
            shutting_down: false,
            submissions: BTreeSet::new(),
            submitted_ready: BTreeMap::new(),
            submitted_labels: BTreeMap::new(),
            merged: Vec::new(),
        })
    }
    pub fn begin_shutdown(&mut self) {
        self.shutting_down = true;
        self.merges
            .retain(|_, intent| self.submissions.contains(&intent.token));
        self.ready
            .retain(|_, intent| self.submissions.contains(&intent.token));
        for jobs in self.label_jobs.values_mut() {
            jobs.retain(|job| self.submissions.contains(&job.intent.token));
        }
        tracing::info!(
            event = "mutations_draining",
            pending = self.submissions.len()
        );
    }

    pub fn has_submissions(&self) -> bool {
        !self.submissions.is_empty()
    }
    /// PRs merged since the last call, so the worker can keep them displayed
    /// briefly instead of treating the merge as a polling failure.
    pub fn take_merged(&mut self) -> Vec<String> {
        std::mem::take(&mut self.merged)
    }
    fn token(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence
    }
    fn failure(&mut self, message: impl Into<String>) -> Failure {
        Failure {
            id: self.token(),
            message: message.into(),
        }
    }
    pub fn publish(&self, context: &Context<'_>) {
        (context.sink)(UiEvent::ActionsChanged(self.state.clone()));
    }
    /// Ends the optimistic display once the worker has handled ReadySaved,
    /// whether it applied the result or rejected it for a changed account/PR.
    pub fn ready_saved(&mut self, pr: &str, context: &Context<'_>) {
        if !self.ready.contains_key(pr)
            && matches!(self.state.ready.get(pr), Some(ReadyProgress::Submitting))
        {
            self.state.ready.remove(pr);
            self.publish(context);
        }
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
            on_green: false,
        })
    }
    pub fn reconcile(&mut self, context: &Context<'_>) {
        if self.viewer.as_deref() != context.viewer {
            let changed = !self.state.merges.is_empty()
                || !self.state.ready.is_empty()
                || !self.state.labels.is_empty();
            self.viewer = context.viewer.map(str::to_owned);
            self.merges.clear();
            self.ready.clear();
            self.loads.clear();
            self.attempts.clear();
            self.load_errors.clear();
            self.completed_labels.clear();
            self.catalogues = context
                .viewer
                .and_then(|viewer| match context.store.label_catalogues(viewer) {
                    Ok(catalogues) => Some(catalogues),
                    Err(error) => {
                        tracing::warn!(event="label_cache_read_failed",error=%error);
                        None
                    }
                })
                .unwrap_or_default();
            self.label_jobs.clear();
            self.state.merges.clear();
            self.state.ready.clear();
            self.state.labels.clear();
            if changed {
                self.publish(context);
            }
        }
        let ready_count = self.state.ready.len();
        self.state
            .ready
            .retain(|id, _| context.prs.get(id).is_none_or(|pr| pr.snapshot.draft));
        if self.state.ready.len() != ready_count {
            self.publish(context);
        }
        // A completed merge is displayed only while its PR remains listed. The
        // worker publishes the row removal first, so forgetting it needs no update.
        self.state.merges.retain(|id, progress| {
            *progress != MergeProgress::Complete || context.prs.contains_key(id)
        });
        self.sync_labels(context);
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
                let failure =
                    self.failure("Merge cancelled: the PR, checks, or action settings changed.");
                self.state.merges.insert(id, MergeProgress::Failed(failure));
            }
            self.publish(context);
        }
        let ready = self
            .merges
            .iter()
            .filter(|(id, _)| {
                self.state.merges.get(*id) == Some(&MergeProgress::WaitingForChecks)
                    && context
                        .prs
                        .get(*id)
                        .is_some_and(|pr| pr.snapshot.check_state == Some(CheckState::Green))
            })
            .map(|(id, intent)| (id.clone(), intent.token))
            .collect::<Vec<_>>();
        for (id, token) in ready {
            self.start_countdown(id, token, context);
            self.publish(context);
        }
        if self.advance_merges(context) {
            self.publish(context);
        }
    }
    pub fn handle(&mut self, command: ActionCommand, context: &Context<'_>) {
        if self.shutting_down
            && !matches!(
                command,
                ActionCommand::Merged { .. }
                    | ActionCommand::ReadyDone { .. }
                    | ActionCommand::LabelDone { .. }
            )
        {
            return;
        }
        let label_request = matches!(&command, ActionCommand::Request(Request::Label { .. }));
        let save_pr = match &command {
            ActionCommand::Request(Request::SaveLabels { pr, .. }) => Some(pr.clone()),
            _ => None,
        };
        match command {
            ActionCommand::Request(request) => {
                if let Err(error) = self.request(request, context) {
                    tracing::warn!(event="pr_action_rejected", error=%error);
                    let failure = self.failure(error.to_string());
                    self.state.error = Some(failure);
                    if let Some(pr) = &save_pr
                        && let Some(labels) = self.state.labels.get_mut(pr)
                    {
                        labels.error = Some(error.to_string());
                    }
                }
            }
            ActionCommand::ReadyChecked { pr, token, result } => {
                if let Some(intent) = self.ready.get(&pr).filter(|i| i.token == token).cloned() {
                    // None: GitHub already shows the PR as ready, which is the requested result.
                    let checked = result.and_then(|(snapshot, github)| {
                        let current = context.prs.get(&pr);
                        if context.viewer == Some(intent.viewer.as_str())
                            && current.is_some_and(|p| {
                                p.snapshot.open
                                    && p.snapshot.draft
                                    && !p.stale
                                    && p.snapshot.head == intent.head
                                    && p.update_id == intent.update
                            })
                            && snapshot.id == pr
                            && snapshot.open
                            && snapshot.head == intent.head
                        {
                            Ok(snapshot.draft.then_some(github))
                        } else {
                            Err("Ready for review cancelled: the PR or account changed.".into())
                        }
                    });
                    match checked {
                        Err(error) => {
                            self.ready.remove(&pr);
                            if context.prs.get(&pr).is_none_or(|p| p.snapshot.draft) {
                                let failure = self.failure(error);
                                self.state.ready.insert(pr, ReadyProgress::Failed(failure));
                            }
                        }
                        Ok(None) => {
                            tracing::info!(event="pr_already_ready_for_review", pr_id=%pr);
                            self.ready.remove(&pr);
                            // Keep displaying the PR as ready until ReadySaved applies it.
                            self.state
                                .ready
                                .insert(pr.clone(), ReadyProgress::Submitting);
                            let _ = context.sender.send(Command::ReadySaved {
                                pr,
                                viewer: intent.viewer,
                            });
                        }
                        Ok(Some(github)) => {
                            self.state
                                .ready
                                .insert(pr.clone(), ReadyProgress::Submitting);
                            self.submissions.insert(token);
                            self.submitted_ready.insert(token, intent.clone());
                            let sender = context.sender.clone();
                            tokio::spawn(async move {
                                let result = github
                                    .ready_for_review(&intent.reference)
                                    .await
                                    .map_err(|e| e.to_string());
                                let _ = sender.send(Command::PrAction(ActionCommand::ReadyDone {
                                    pr,
                                    token,
                                    result,
                                }));
                            });
                        }
                    }
                }
            }
            ActionCommand::ReadyDone { pr, token, result } => {
                self.submissions.remove(&token);
                if let Some(intent) = self.submitted_ready.remove(&token) {
                    let current = self.ready.get(&pr).is_some_and(|i| i.token == token);
                    if current {
                        self.ready.remove(&pr);
                    }
                    match result {
                        Ok(()) => {
                            tracing::info!(event="pr_ready_for_review", pr_id=%pr);
                            // Its Submitting progress keeps the PR displayed as ready until
                            // ReadySaved applies the result, so the row never flashes back.
                            let _ = context.sender.send(Command::ReadySaved {
                                pr,
                                viewer: intent.viewer,
                            });
                        }
                        Err(error) => {
                            tracing::warn!(event="ready_for_review_failed", pr_id=%pr, error=%error);
                            if current && context.prs.get(&pr).is_none_or(|p| p.snapshot.draft) {
                                let failure = self.failure(error);
                                self.state.ready.insert(pr, ReadyProgress::Failed(failure));
                            }
                        }
                    }
                }
            }
            ActionCommand::Tick {
                pr,
                token,
                remaining,
            } => {
                if self.merges.get(&pr).is_some_and(|i| i.token == token)
                    && matches!(
                        self.state.merges.get(&pr),
                        Some(MergeProgress::Countdown(_))
                    )
                {
                    if remaining > 0 {
                        self.state
                            .merges
                            .insert(pr, MergeProgress::Countdown(remaining));
                    } else {
                        match self.merge_blocker(&pr, context) {
                            Some(number) => {
                                self.queue_merge(pr, number);
                            }
                            None => self.check_merge(&pr, context),
                        }
                    }
                }
            }
            ActionCommand::MergeChecked { pr, token, result } => {
                if let Some(intent) = self.merges.get(&pr).filter(|i| i.token == token).cloned() {
                    let checked = result.and_then(|(snapshot, github)| {
                        let expected = context.config.repositories.get(&snapshot.repo).and_then(|r| r.reviewers.as_deref());
                        let fresh = transition_with_grace(*snapshot, Some(&intent.pr), expected, chrono::Utc::now().timestamp(), context.config.settle_seconds, context.config.review_start_grace_seconds);
                        if intent.valid(context, &self.state, Kind::Merge) && intent.preferences.allows(Kind::Merge, &fresh) && fresh.snapshot.head == intent.pr.snapshot.head && !retargeted(&intent.pr.snapshot, &fresh.snapshot) && fresh.update_id == intent.pr.update_id && (!intent.on_green || (fresh.snapshot.check_state == Some(CheckState::Green) && context.prs.get(&pr).is_some_and(|current| current.snapshot.check_state == Some(CheckState::Green)))) {
                            Ok(github)
                        } else { Err("Merge cancelled: the commit, review state, checks, or action settings changed.".into()) }
                    });
                    match checked {
                        Err(error) => {
                            self.merges.remove(&pr);
                            let failure = self.failure(error);
                            self.state.merges.insert(pr, MergeProgress::Failed(failure));
                        }
                        Ok(github) => {
                            self.state.merges.insert(pr.clone(), MergeProgress::Merging);
                            self.submissions.insert(token);
                            let sender = context.sender.clone();
                            tokio::spawn(async move {
                                let result = async {
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
                self.submissions.remove(&token);
                // GitHub merged the PR even if an account change discarded its intent.
                if result.is_ok() {
                    tracing::info!(event="pr_merged", pr_id=%pr);
                    self.merged.push(pr.clone());
                    let _ = context.sender.send(Command::Refresh);
                }
                if self.merges.get(&pr).is_some_and(|i| i.token == token) {
                    self.merges.remove(&pr);
                    match result {
                        Ok(()) => {
                            self.state.merges.insert(pr, MergeProgress::Complete);
                        }
                        Err(error) => {
                            tracing::warn!(event="merge_failed", pr_id=%pr, error=%error);
                            let failure = self.failure(error);
                            self.state.merges.insert(pr, MergeProgress::Failed(failure));
                        }
                    }
                }
            }
            ActionCommand::LabelsLoaded {
                repo,
                token,
                result,
            } => {
                if self.loads.get(&repo) == Some(&token) && self.viewer.as_deref() == context.viewer
                {
                    self.loads.remove(&repo);
                    match result {
                        Ok(labels) => {
                            let catalogue = LabelCatalogue {
                                labels,
                                fetched_at: chrono::Utc::now().timestamp(),
                            };
                            if let Some(viewer) = context.viewer
                                && let Err(error) = context
                                    .store
                                    .save_label_catalogue(viewer, &repo, &catalogue)
                            {
                                tracing::warn!(event="label_cache_save_failed",repo=%repo,error=%error);
                                self.load_errors.insert(
                                    repo.clone(),
                                    format!("Could not save label cache: {error}"),
                                );
                            } else {
                                self.load_errors.remove(&repo);
                            }
                            tracing::info!(event="repository_labels_refreshed",repo=%repo,count=catalogue.labels.len());
                            self.catalogues.insert(repo, catalogue);
                        }
                        Err(error) => {
                            tracing::warn!(event="labels_load_failed",repo=%repo,error=%error);
                            self.load_errors.insert(repo, error);
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
                        Ok(_) if !job.intent.valid(context, &self.state, Kind::Label) => self
                            .finish_label(
                                &pr,
                                token,
                                Err("Label action cancelled: PR or settings changed.".into()),
                                context,
                            ),
                        Ok(github) => {
                            self.submissions.insert(token);
                            self.submitted_labels.insert(token, job.clone());
                            let sender = context.sender.clone();
                            tokio::spawn(async move {
                                let result = async {
                                    // Authentication finished before LabelChecked; submit
                                    // directly after the worker revalidates the intent.
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
                self.submissions.remove(&token);
                self.finish_label(&pr, token, result, context)
            }
        }
        self.reconcile(context);
        self.publish(context);
        if label_request {
            (context.sink)(UiEvent::LabelRequestHandled);
        }
    }

    /// Removes only the failure the user saw; in-progress actions and newer
    /// failures are left untouched.
    fn dismiss(&mut self, error: DisplayedError) {
        let (pr, dismissed) = match error {
            DisplayedError::Ready { pr, id } => {
                let shown = matches!(
                    self.state.ready.get(&pr),
                    Some(ReadyProgress::Failed(current)) if current.id == id
                );
                let dismissed = shown && self.state.ready.remove(&pr).is_some();
                (Some(pr), dismissed)
            }
            DisplayedError::Merge { pr, id } => {
                let shown = matches!(
                    self.state.merges.get(&pr),
                    Some(MergeProgress::Failed(current)) if current.id == id
                );
                let dismissed = shown && self.state.merges.remove(&pr).is_some();
                (Some(pr), dismissed)
            }
            DisplayedError::Request(id) => {
                let shown = self
                    .state
                    .error
                    .as_ref()
                    .is_some_and(|error| error.id == id);
                (None, shown && self.state.error.take().is_some())
            }
        };
        tracing::info!(event = "action_error_dismissed", pr_id = ?pr, dismissed);
    }
    fn request(&mut self, request: Request, context: &Context<'_>) -> Result<()> {
        // Dismissing one error must not clear a different, unseen request error.
        if !matches!(request, Request::DismissError(_)) {
            self.state.error = None;
        }
        match request {
            Request::ReadyForReview { pr, head, update } => {
                ensure!(
                    !self.ready.contains_key(&pr),
                    "Ready for review is already pending"
                );
                let current = context
                    .prs
                    .get(&pr)
                    .context("This PR is no longer in the active list")?;
                ensure!(
                    current.snapshot.open && current.snapshot.draft && !current.stale,
                    "Ready for review requires a fresh draft PR"
                );
                ensure!(
                    current.snapshot.head == head && current.update_id == update,
                    "Ready for review cancelled: the displayed PR or review changed"
                );
                let intent = ReadyIntent {
                    token: self.token(),
                    reference: PrRef {
                        id: pr.clone(),
                        repo: current.snapshot.repo.clone(),
                        number: current.snapshot.number,
                    },
                    viewer: context
                        .viewer
                        .context("Waiting for GitHub authentication")?
                        .into(),
                    head,
                    update,
                };
                let token = intent.token;
                let reference = intent.reference.clone();
                let viewer = intent.viewer.clone();
                self.ready.insert(pr.clone(), intent);
                self.state.ready.insert(pr.clone(), ReadyProgress::Checking);
                let sender = context.sender.clone();
                let config = context.config.clone();
                tokio::spawn(async move {
                    let result = async {
                        let github = Github::new(&config)?;
                        ensure!(github.viewer().await? == viewer, "GitHub account changed");
                        let snapshot = github.snapshot(&reference).await?;
                        ensure!(github.viewer().await? == viewer, "GitHub account changed");
                        Ok((Box::new(snapshot), github))
                    }
                    .await
                    .map_err(|e: anyhow::Error| e.to_string());
                    let _ = sender.send(Command::PrAction(ActionCommand::ReadyChecked {
                        pr,
                        token,
                        result,
                    }));
                });
            }
            Request::Configure { repo, change } => {
                let mut preferences = self.state.preferences(&repo);
                preferences.apply(&change);
                context.store.save_action_preferences(&repo, &preferences)?;
                self.state.preferences.insert(repo_key(&repo), preferences);
                tracing::info!(event="action_settings_saved", repo=%repo);
            }
            Request::Merge {
                pr,
                head,
                update,
                on_green,
            } => {
                ensure!(
                    !self.merges.contains_key(&pr),
                    "A merge is already pending for this PR"
                );
                let mut intent = self.intent(&pr, Kind::Merge, context)?;
                intent.on_green = on_green;
                ensure!(
                    !on_green
                        || matches!(
                            intent.pr.snapshot.check_state,
                            Some(CheckState::Running | CheckState::Green)
                        ),
                    "Merge cancelled: checks are unavailable or failed"
                );
                ensure!(
                    head == intent.pr.snapshot.head && !head.is_empty(),
                    "The PR commit changed; refresh before merging"
                );
                ensure!(
                    update == intent.pr.update_id,
                    "The PR review changed; refresh before merging"
                );
                let token = intent.token;
                self.merges.insert(pr.clone(), intent);
                if on_green {
                    tracing::info!(event="merge_waiting_for_checks", pr_id=%pr);
                    self.state
                        .merges
                        .insert(pr, MergeProgress::WaitingForChecks);
                } else {
                    self.start_countdown(pr, token, context);
                }
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
                self.refresh_catalogues(context, Some(&intent.pr.snapshot.repo));
            }
            Request::Label { pr, name, selected } => {
                self.intent(&pr, Kind::Label, context)?;
                let labels = self
                    .state
                    .labels
                    .get_mut(&pr)
                    .context("Load the PR labels first")?;
                ensure!(
                    !labels.pending.contains(&name),
                    "Label update already pending"
                );
                let label = labels
                    .items
                    .iter_mut()
                    .find(|label| label.name == name)
                    .context("Label no longer exists")?;
                if label.selected == selected {
                    return Ok(());
                }
                label.selected = selected;
                let applied = self
                    .completed_labels
                    .get(&(pr.clone(), name.clone()))
                    .copied()
                    .unwrap_or_else(|| {
                        context.prs[&pr]
                            .snapshot
                            .labels
                            .iter()
                            .any(|label| label.name == name)
                    });
                if selected == applied {
                    labels.unsaved.remove(&name);
                } else {
                    labels.unsaved.insert(name);
                }
                if labels.unsaved.is_empty() {
                    labels.error = None;
                }
            }
            Request::DismissError(error) => self.dismiss(error),
            Request::SaveLabels { pr, name } => {
                let names: Vec<String> = {
                    let labels = self
                        .state
                        .labels
                        .get(&pr)
                        .context("Load the PR labels first")?;
                    match name {
                        Some(name) if labels.unsaved.contains(&name) => vec![name],
                        Some(_) => Vec::new(),
                        None => labels.unsaved.iter().cloned().collect(),
                    }
                };
                for name in names {
                    let intent = self.intent(&pr, Kind::Label, context)?;
                    let labels = self.state.labels.get_mut(&pr).unwrap();
                    let label = labels
                        .items
                        .iter()
                        .find(|item| item.name == name)
                        .context("Label no longer exists")?;
                    let selected = label.selected;
                    let color = label.color.clone();
                    labels.unsaved.remove(&name);
                    labels.pending.insert(name.clone());
                    if labels.unsaved.is_empty() {
                        labels.error = None;
                    }
                    let jobs = self.label_jobs.entry(pr.clone()).or_default();
                    jobs.push_back(LabelJob {
                        intent,
                        name,
                        selected,
                        color,
                    });
                    if jobs.len() == 1 {
                        self.start_label(&pr, context);
                    }
                }
            }
        }
        Ok(())
    }

    fn start_countdown(&mut self, pr: String, token: u64, context: &Context<'_>) {
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
    /// The PR number this merge must wait for: a same-base merge being checked
    /// or submitted, or an earlier request whose countdown or queue is pending.
    /// GitHub rejects concurrent merges with "Base branch was modified", and
    /// mutations are never retried, so each base merges serially in request
    /// order regardless of which countdown timer is delivered first.
    fn merge_blocker(&self, pr: &str, context: &Context<'_>) -> Option<u64> {
        let intent = self.merges.get(pr)?;
        let lane = intent.lane(context);
        self.merges
            .iter()
            .filter(|(other, active)| {
                *other != pr
                    && match self.state.merges.get(*other) {
                        Some(MergeProgress::Checking | MergeProgress::Merging) => true,
                        Some(MergeProgress::Countdown(_) | MergeProgress::Queued(_)) => {
                            active.token < intent.token
                        }
                        _ => false,
                    }
                    && shares_lane(&active.lane(context), &lane)
            })
            .min_by_key(|(other, active)| {
                (
                    !matches!(
                        self.state.merges.get(*other),
                        Some(MergeProgress::Checking | MergeProgress::Merging)
                    ),
                    active.token,
                )
            })
            .map(|(_, active)| active.pr.snapshot.number)
    }
    fn queue_merge(&mut self, pr: String, behind: u64) -> bool {
        let progress = MergeProgress::Queued(behind);
        if self.state.merges.get(&pr) == Some(&progress) {
            return false;
        }
        tracing::info!(event="merge_queued", pr_id=%pr, behind);
        self.state.merges.insert(pr, progress);
        true
    }
    /// Starts queued merges in request order once their base branch is free.
    /// Each one still runs the full final validation against the new base.
    fn advance_merges(&mut self, context: &Context<'_>) -> bool {
        if self.shutting_down {
            return false;
        }
        let mut queued = self
            .merges
            .iter()
            .filter(|(id, _)| matches!(self.state.merges.get(*id), Some(MergeProgress::Queued(_))))
            .map(|(id, intent)| (intent.token, id.clone()))
            .collect::<Vec<_>>();
        queued.sort_unstable();
        let mut changed = false;
        for (_, id) in queued {
            changed |= match self.merge_blocker(&id, context) {
                Some(number) => self.queue_merge(id, number),
                None => {
                    self.check_merge(&id, context);
                    true
                }
            };
        }
        changed
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
                // A preceding merge moves the base branch. Reads may repeat, but
                // the merge itself is submitted once even if this stays unknown.
                let poll = async {
                    while !github.mergeability_known(&intent.reference()).await? {
                        tokio::time::sleep(MERGEABILITY_POLL).await;
                    }
                    anyhow::Ok(())
                };
                match tokio::time::timeout(MERGEABILITY_WAIT, poll).await {
                    Ok(result) => result?,
                    Err(_) => tracing::info!(event="merge_mergeability_unknown", pr_id=%pr),
                }
                let snapshot = github.snapshot(&intent.reference()).await?;
                // Remain cancellable while checking authentication, then let the
                // worker revalidate its latest PR state and settings before merging.
                ensure!(
                    github.viewer().await? == intent.viewer,
                    "GitHub account changed; merge cancelled"
                );
                Ok((Box::new(snapshot), github))
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
        if self.shutting_down {
            return;
        }
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
                let github = Github::new(&config)?;
                ensure!(
                    github.viewer().await? == job.intent.viewer,
                    "GitHub account changed; label action cancelled"
                );
                Ok(github)
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
        let current = self
            .label_jobs
            .get(pr)
            .and_then(|jobs| jobs.front())
            .is_some_and(|job| job.intent.token == token);
        let queued = if current {
            self.label_jobs.get_mut(pr).and_then(VecDeque::pop_front)
        } else {
            None
        };
        let Some(job) = self.submitted_labels.remove(&token).or(queued) else {
            return;
        };
        // The result outlives the UI intent. Always report it, but never mutate
        // a new account's picker or remove its pending toggle for the same PR.
        match &result {
            Err(error) => {
                tracing::warn!(event="label_update_failed", pr_id=%pr, viewer=%job.intent.viewer, error=%error)
            }
            Ok(()) => {
                tracing::info!(event="label_updated", pr_id=%pr, viewer=%job.intent.viewer, label=%job.name, selected=job.selected);
                let _ = context.sender.send(Command::LabelSaved {
                    pr: pr.into(),
                    viewer: job.intent.viewer.clone(),
                    label: crate::model::PrLabel {
                        name: job.name.clone(),
                        color: job.color.clone(),
                    },
                    selected: job.selected,
                });
            }
        }
        if current && context.viewer == Some(job.intent.viewer.as_str()) {
            if let Some(labels) = self.state.labels.get_mut(pr) {
                labels.pending.remove(&job.name);
                if let Err(error) = result {
                    labels.unsaved.insert(job.name.clone());
                    labels.error = Some(error);
                } else {
                    self.completed_labels
                        .insert((pr.into(), job.name.clone()), job.selected);
                }
            }
            self.start_label(pr, context);
        }
    }
}
