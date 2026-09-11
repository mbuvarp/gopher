use crate::{
    config::Config,
    github::{Github, PrRef},
    model::*,
    reviewers,
    store::Store,
};
use anyhow::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

const IGNORED_CHECK_INTERVAL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug)]
pub enum UiEvent {
    Stopped,
    HotkeysLoaded {
        preferences: crate::hotkeys::Preferences,
        error: Option<String>,
    },
    HotkeysSaved(std::result::Result<(), String>),
    /// Sent after the action-state event for a label request, including rejection.
    LabelRequestHandled,
    ActionsChanged(crate::actions::ActionState),
    IgnoredUpdated {
        prs: Vec<PullRequest>,
        error: Option<String>,
        loading: bool,
    },
    Updated {
        prs: Vec<PullRequest>,
        error: Option<String>,
        loading: bool,
    },
    Notify {
        review: bool,
        id: String,
        title: String,
        body: String,
    },
    DismissNotifications(Vec<String>),
    /// Sent after Updated so notification navigation sees persisted acknowledgement state.
    ShowPopover {
        pr: Option<String>,
    },
    Open {
        url: String,
        pr: String,
        update: String,
    },
}
pub enum Command {
    SaveHotkeys(crate::hotkeys::Preferences),
    PrAction(ActionCommand),
    LabelSaved {
        pr: String,
        viewer: String,
        label: crate::model::PrLabel,
        selected: bool,
    },
    Refresh,
    Acknowledge {
        pr: String,
        update: String,
        checked: bool,
    },
    Ignore(String),
    ShowIgnored,
    CheckIgnored,
    Restore(String),
    IgnoredDetailsLoaded(std::result::Result<Vec<Snapshot>, String>),
    NotificationAction {
        id: String,
        open: bool,
        reveal: bool,
    },
    NotificationDelivered(String),
    NotificationFailed(String),
    Shutdown,
    ShutdownComplete,
    PollComplete(std::result::Result<Batch, String>),
    CheckLabels,
    CredentialsProbed,
}
pub struct Batch {
    viewer: String,
    references: Vec<PrRef>,
    discovered: bool,
    results: Vec<(String, std::result::Result<Snapshot, String>)>,
}
pub type Sink = Arc<dyn Fn(UiEvent) + Send + Sync>;

struct StopNotice(Sink);
impl Drop for StopNotice {
    fn drop(&mut self) {
        (self.0)(UiEvent::Stopped);
    }
}

pub struct Worker {
    pub sender: UnboundedSender<Command>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn start(directory: PathBuf, config: Config, sink: Sink) -> Result<Worker> {
    let (sender, receiver) = unbounded_channel();
    let task_sender = sender.clone();
    let thread = std::thread::Builder::new()
        .name("gopher-worker".into())
        .spawn(move || {
            // Notify the event loop even if the worker panics while draining.
            let _stopped = StopNotice(sink.clone());
            let run = || -> Result<()> {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()?;
                runtime.block_on(run(
                    directory,
                    config,
                    receiver,
                    task_sender,
                    sink.clone(),
                    IGNORED_CHECK_INTERVAL,
                ))
            };
            if let Err(error) = run() {
                tracing::error!(event="worker_failed", error=%error);
                sink(UiEvent::Updated {
                    prs: vec![],
                    error: Some(error.to_string()),
                    loading: false,
                });
                sink(UiEvent::Notify {
                    review: false,
                    id: "gopher-worker-error".into(),
                    title: "Gopher needs attention".into(),
                    body: error.to_string(),
                });
            }
        })?;
    Ok(Worker {
        sender,
        thread: Some(thread),
    })
}

async fn fetch(
    config: Config,
    references: Vec<PrRef>,
    discover: bool,
    old_viewer: Option<String>,
    ignored: BTreeSet<String>,
) -> Result<Batch> {
    let github = Github::new(&config)?;
    let viewer = github.viewer().await?;
    let discovered = discover || old_viewer.as_deref() != Some(&viewer);
    let references = if discovered {
        let mut found: BTreeMap<_, _> = github
            .discover(&viewer)
            .await?
            .into_iter()
            .map(|r| (r.id.clone(), r))
            .collect();
        // A missing search hit is not evidence of closure. Keep monitoring known
        // PRs until their direct snapshot confirms closure, scoped to this account.
        if old_viewer.as_deref() == Some(&viewer) {
            for reference in references {
                if !found.contains_key(&reference.id) {
                    tracing::info!(event = "discovery_omission_retained", repo = %reference.repo, pr = reference.number);
                    found.insert(reference.id.clone(), reference);
                }
            }
        }
        found.into_values().collect()
    } else {
        references
    };
    let references: Vec<_> = references
        .into_iter()
        .filter(|r| {
            !ignored.contains(&r.id) && !config.repositories.get(&r.repo).is_some_and(|c| c.ignore)
        })
        .collect();
    let mut tasks = tokio::task::JoinSet::new();
    let mut pending = references.clone().into_iter();
    let mut results = Vec::new();
    loop {
        while tasks.len() < 3 {
            let Some(reference) = pending.next() else {
                break;
            };
            let github = github.clone();
            tasks.spawn(async move {
                let result = github
                    .snapshot_unverified(&reference)
                    .await
                    .map_err(|e| e.to_string());
                (reference.id, result)
            });
        }
        let Some(result) = tasks.join_next().await else {
            break;
        };
        results.push(result?);
    }
    let ids = results
        .iter()
        .filter(|(_, result)| result.is_ok())
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let heads = github.final_heads(&ids, &viewer).await?;
    for (id, result) in &mut results {
        if let Ok(snapshot) = result
            && !heads
                .get(id)
                .is_some_and(|(head, open)| head == &snapshot.head && *open == snapshot.open)
        {
            *result = Err("PR changed while fetching checks; retrying next poll".into());
        }
    }
    Ok(Batch {
        viewer,
        references,
        discovered,
        results,
    })
}

async fn run(
    directory: PathBuf,
    config: Config,
    mut receiver: UnboundedReceiver<Command>,
    sender: UnboundedSender<Command>,
    sink: Sink,
    ignored_interval: Duration,
) -> Result<()> {
    let mut store = Store::open(&directory)?;
    let mut pr_actions = action_worker::Coordinator::new(&store)?;
    let mut ignored = store.ignored()?;
    let mut prs: BTreeMap<_, _> = store
        .load()?
        .into_iter()
        .filter(|p| !ignored.contains(&p.snapshot.id))
        .map(|p| (p.snapshot.id.clone(), p))
        .collect();
    let mut references: Vec<_> = prs
        .values()
        .map(|p| PrRef {
            id: p.snapshot.id.clone(),
            repo: p.snapshot.repo.clone(),
            number: p.snapshot.number,
        })
        .collect();
    let mut error: Option<String> = None;
    let mut viewer = store.viewer()?;
    let mut discovered_at: Option<Instant> = None;
    let mut deadline = Instant::now();
    let mut polling = false;
    let mut ignored_loading = false;
    let mut ignored_checked = false;
    let mut ignored_deadline = Instant::now();
    let mut ignored_requested = false;
    let mut rediscover_after_poll = false;
    let mut invalidated_poll_ids = BTreeSet::new();
    let mut failures = 0_u32;
    let mut in_flight_notifications = BTreeSet::new();
    let mut dismiss_after_delivery = BTreeSet::new();
    // The first UI snapshot must include cached labels, without waiting for a poll.
    pr_actions.reconcile(&action_worker::Context {
        store: &store,
        prs: &prs,
        viewer: viewer.as_deref(),
        config: &config,
        sender: &sender,
        sink: &sink,
    });
    sink(UiEvent::ActionsChanged(pr_actions.state.clone()));
    sink(UiEvent::Updated {
        prs: prs.values().cloned().collect(),
        error: None,
        loading: false,
    });
    match store.hotkeys() {
        Ok(preferences) => sink(UiEvent::HotkeysLoaded {
            preferences,
            error: None,
        }),
        Err(error) => sink(UiEvent::HotkeysLoaded {
            preferences: Default::default(),
            error: Some(format!("Cannot load shortcuts: {error}")),
        }),
    }
    let mut label_deadline = Instant::now();
    let mut shutting_down = false;
    loop {
        // Keep ordinary deadlines intact so a credential switch can lift an old
        // quota wait. Ignored identities use GraphQL; snapshots need both quotas.
        let poll_ready = deadline.max(Instant::now() + Github::cooldown(&config, None));
        let ignored_ready =
            ignored_deadline.max(Instant::now() + Github::cooldown(&config, Some("graphql")));
        let command = tokio::select! {
            _ = tokio::time::sleep_until(label_deadline.into()), if !shutting_down => Command::CheckLabels,
            command = receiver.recv() => match command { Some(c)=>c,None=>break },
            _ = tokio::time::sleep_until(ignored_ready.into()), if !shutting_down && !ignored_loading => Command::CheckIgnored,
            _ = tokio::time::sleep_until(poll_ready.into()), if !shutting_down && !polling => {
                polling = true;
                sink(UiEvent::Updated {
                    prs: prs.values().cloned().collect(),
                    error: error.clone(),
                    loading: true,
                });
                let config = config.clone();
                let references = references.clone();
                let viewer = viewer.clone();
                let ignored = ignored.clone();
                let sender = sender.clone();
                let discover = discovered_at.is_none_or(|time|time.elapsed().as_secs() >= config.discovery_seconds);
                tokio::spawn(async move {
                    let result = fetch(config,references,discover,viewer,ignored).await.map_err(|e|e.to_string());
                    let _ = sender.send(Command::PollComplete(result));
                });
                continue;
            }
        };
        if shutting_down
            && !matches!(
                &command,
                Command::Shutdown
                    | Command::ShutdownComplete
                    | Command::LabelSaved { .. }
                    | Command::PrAction(
                        ActionCommand::Merged { .. } | ActionCommand::LabelDone { .. }
                    )
            )
        {
            continue;
        }
        let mut reveal_target = None;
        match command {
            Command::SaveHotkeys(preferences) => {
                let result = store.save_hotkeys(&preferences).map_err(|e| e.to_string());
                if let Err(error) = &result {
                    tracing::error!(event = "hotkeys_save_failed", error);
                }
                sink(UiEvent::HotkeysSaved(result));
                continue;
            }
            Command::CredentialsProbed => {}
            Command::CheckLabels => {
                if !Github::cooldown(&config, None).is_zero() {
                    let config = config.clone();
                    let sender = sender.clone();
                    tokio::spawn(async move {
                        let command = match Github::probe_credentials(&config).await {
                            Ok(true) => Command::Refresh,
                            Ok(false) => Command::CredentialsProbed,
                            Err(error) => {
                                tracing::debug!(event="credential_probe_failed",error=%error);
                                Command::CredentialsProbed
                            }
                        };
                        let _ = sender.send(command);
                    });
                }
                label_deadline = Instant::now() + Duration::from_secs(30);
                pr_actions.refresh_catalogues(
                    &action_worker::Context {
                        store: &store,
                        prs: &prs,
                        viewer: viewer.as_deref(),
                        config: &config,
                        sender: &sender,
                        sink: &sink,
                    },
                    None,
                );
                continue;
            }
            Command::LabelSaved {
                pr: id,
                viewer: account,
                label,
                selected,
            } => {
                if viewer.as_deref() == Some(account.as_str())
                    && let Some(pr) = prs.get_mut(&id)
                {
                    pr_actions.label_saved(&id, &label.name);
                    pr.snapshot
                        .labels
                        .retain(|existing| existing.name != label.name);
                    if selected {
                        pr.snapshot.labels.push(label);
                    }
                    store.save(pr)?;
                    // A poll started before the mutation must not replace its result.
                    if polling {
                        invalidated_poll_ids.insert(id);
                        rediscover_after_poll = true;
                    }
                    deadline = Instant::now();
                }
            }
            Command::PrAction(command) => {
                pr_actions.handle(
                    command,
                    &action_worker::Context {
                        store: &store,
                        prs: &prs,
                        viewer: viewer.as_deref(),
                        config: &config,
                        sender: &sender,
                        sink: &sink,
                    },
                );
            }
            Command::Shutdown => {
                if !shutting_down {
                    shutting_down = true;
                    pr_actions.begin_shutdown();
                    if !pr_actions.has_submissions() {
                        let _ = sender.send(Command::ShutdownComplete);
                    }
                }
                continue;
            }
            Command::ShutdownComplete => {
                if shutting_down && !pr_actions.has_submissions() {
                    break;
                }
                continue;
            }
            Command::Refresh => {
                deadline = Instant::now();
                discovered_at = None;
            }
            Command::ShowIgnored => {
                ignored_requested = true;
                if !ignored_checked && !ignored_loading {
                    let _ = sender.send(Command::CheckIgnored);
                }
                sink(UiEvent::IgnoredUpdated {
                    prs: store.load_ignored()?,
                    error: None,
                    loading: ignored_loading,
                });
            }
            Command::CheckIgnored => {
                if ignored_loading {
                    continue;
                }
                ignored_deadline = Instant::now() + ignored_interval;
                if ignored.is_empty() {
                    continue;
                }
                ignored_loading = true;
                let ids: Vec<_> = ignored.iter().cloned().collect();
                tracing::debug!(event = "ignored_check_started", count = ids.len());
                let config = config.clone();
                let sender = sender.clone();
                tokio::spawn(async move {
                    let result = async { Github::new(&config)?.ignored_details(&ids).await }
                        .await
                        .map_err(|e| e.to_string());
                    let _ = sender.send(Command::IgnoredDetailsLoaded(result));
                });
                sink(UiEvent::IgnoredUpdated {
                    prs: store.load_ignored()?,
                    error: None,
                    loading: true,
                });
            }
            Command::IgnoredDetailsLoaded(result) => {
                ignored_loading = false;
                ignored_deadline = Instant::now() + ignored_interval;
                let lookup_error = match result {
                    Ok(snapshots) => {
                        ignored_checked = true;
                        tracing::info!(
                            event = "ignored_check_complete",
                            checked = snapshots.len(),
                            closed = snapshots.iter().filter(|p| !p.open).count()
                        );
                        for snapshot in snapshots {
                            store.update_ignored_status(&snapshot)?;
                        }
                        None
                    }
                    Err(message) => {
                        ignored_checked = false;
                        tracing::warn!(event="ignored_details_failed",error=%message);
                        Some(message)
                    }
                };
                sink(UiEvent::IgnoredUpdated {
                    prs: store.load_ignored()?,
                    error: lookup_error,
                    loading: false,
                });
            }
            Command::Restore(id) => {
                if store.restore(&id)? {
                    ignored.remove(&id);
                    if polling {
                        invalidated_poll_ids.insert(id);
                        rediscover_after_poll = true;
                    }
                    discovered_at = None;
                    deadline = Instant::now();
                }
                sink(UiEvent::IgnoredUpdated {
                    prs: store.load_ignored()?,
                    error: None,
                    loading: ignored_loading,
                });
            }
            Command::Acknowledge {
                pr,
                update,
                checked,
            } => {
                if let Some(pr) = prs.get_mut(&pr)
                    && store.acknowledge(pr, &update, checked)?
                    && checked
                {
                    let ids = store.notification_ids(&pr.snapshot.id, Some(&update))?;
                    dismiss_after_delivery.extend(
                        ids.iter()
                            .filter(|id| in_flight_notifications.contains(*id))
                            .cloned(),
                    );
                    sink(UiEvent::DismissNotifications(ids));
                }
            }
            Command::Ignore(id) => {
                let notifications = store.ignore(&id)?;
                ignored.insert(id.clone());
                ignored_checked = false;
                if polling {
                    invalidated_poll_ids.insert(id.clone());
                }
                prs.remove(&id);
                references.retain(|r| r.id != id);
                dismiss_after_delivery.extend(
                    notifications
                        .iter()
                        .filter(|id| in_flight_notifications.contains(*id))
                        .cloned(),
                );
                sink(UiEvent::DismissNotifications(notifications));
                if ignored_requested {
                    sink(UiEvent::IgnoredUpdated {
                        prs: store.load_ignored()?,
                        error: None,
                        loading: ignored_loading,
                    });
                }
            }
            Command::NotificationAction { id, open, reveal } => {
                let target = store.notification_target(&id)?;
                if reveal {
                    reveal_target = Some(target.as_ref().map(|(pr, _, _)| pr.clone()));
                }
                if let Some((pr, update, url)) = target {
                    if open {
                        sink(UiEvent::Open { url, pr, update });
                    } else {
                        if let Some(pr) = prs.get_mut(&pr) {
                            store.acknowledge(pr, &update, true)?;
                        }
                        if in_flight_notifications.contains(&id) {
                            dismiss_after_delivery.insert(id.clone());
                        }
                        sink(UiEvent::DismissNotifications(vec![id.clone()]));
                    }
                    tracing::info!(event="notification_action", notification=%id, open, reveal);
                }
            }
            Command::NotificationDelivered(id) => {
                store.mark_delivered(&id)?;
                in_flight_notifications.remove(&id);
                // A native add request can complete after the user dismissed its update.
                if dismiss_after_delivery.remove(&id) {
                    sink(UiEvent::DismissNotifications(vec![id]));
                }
            }
            Command::NotificationFailed(id) => {
                in_flight_notifications.remove(&id);
                dismiss_after_delivery.remove(&id);
            }
            Command::PollComplete(result) => {
                polling = false;
                let mut new_error = None;
                match result {
                    Err(message) => {
                        for pr in prs.values_mut() {
                            pr.stale = true;
                        }
                        new_error = Some(message);
                    }
                    Ok(batch) => {
                        if store.set_viewer(&batch.viewer)? {
                            prs.clear();
                            in_flight_notifications.clear();
                        }
                        viewer = Some(batch.viewer);
                        references = batch
                            .references
                            .into_iter()
                            .filter(|r| !ignored.contains(&r.id))
                            .collect();
                        if batch.discovered {
                            discovered_at = Some(Instant::now());
                            let ids: BTreeSet<_> =
                                references.iter().map(|r| r.id.clone()).collect();
                            prs.retain(|id, _| ids.contains(id));
                            store.retain(&ids)?;
                        }
                        let now = chrono::Utc::now().timestamp();
                        for (id, result) in batch.results {
                            // The user may have ignored this PR while its request was running.
                            if ignored.contains(&id) || invalidated_poll_ids.contains(&id) {
                                continue;
                            }
                            match result {
                                Err(message) => {
                                    if let Some(pr) = prs.get_mut(&id) {
                                        pr.stale = true;
                                        pr.error = Some(message.clone());
                                    }
                                    let reference = references.iter().find(|r| r.id == id);
                                    tracing::warn!(event="poll_failed", pr_id=%id,
                                        repo=reference.map(|r|r.repo.as_str()),pr=reference.map(|r|r.number),error=%message);
                                    if !message.starts_with("PR changed") {
                                        new_error = Some(message);
                                    }
                                }
                                Ok(snapshot) if !snapshot.open => {
                                    prs.remove(&id);
                                    references.retain(|r| r.id != id);
                                }
                                Ok(snapshot) => {
                                    let previous = prs.get(&id);
                                    let expected = config
                                        .repositories
                                        .get(&snapshot.repo)
                                        .and_then(|r| r.reviewers.as_deref());
                                    let pr = transition(
                                        snapshot,
                                        previous,
                                        expected,
                                        now,
                                        config.settle_seconds,
                                    );
                                    if previous.is_none_or(|old| {
                                        old.update_id != pr.update_id || old.agents != pr.agents
                                    }) {
                                        tracing::info!(event="review_state",repo=%pr.snapshot.repo,pr=pr.snapshot.number,state=?pr.state);
                                        if let Some(old) = previous {
                                            for agent in old.agents.iter().filter(|a| {
                                                a.verdict == Verdict::Skipped
                                                    && !pr
                                                        .agents
                                                        .iter()
                                                        .any(|current| current.agent == a.agent)
                                            }) {
                                                tracing::info!(event="reviewer_not_participating",repo=%pr.snapshot.repo,pr=pr.snapshot.number,agent=?agent.agent,reason="Previously skipped reviewer has no current activity");
                                            }
                                        }
                                        for agent in &pr.agents {
                                            tracing::info!(event="review_evidence",repo=%pr.snapshot.repo,pr=pr.snapshot.number,agent=?agent.agent,verdict=?agent.verdict,reason=%agent.reason,run_id=%agent.run_id);
                                        }
                                    }
                                    store.save(&pr)?;
                                    if config.notifications
                                        && pr.needs_attention()
                                        && let Some(id) = store.notification(&pr)?
                                        && in_flight_notifications.insert(id.clone())
                                    {
                                        sink(UiEvent::Notify {
                                            review: true,
                                            id,
                                            title: format!(
                                                "{} #{} · {}",
                                                pr.snapshot.repo,
                                                pr.snapshot.number,
                                                pr.state.label()
                                            ),
                                            body: pr.snapshot.title.clone(),
                                        });
                                    }
                                    prs.insert(id, pr);
                                }
                            }
                        }
                        store.retain(&prs.keys().cloned().collect())?;
                    }
                }
                if new_error != error {
                    if let Some(message) = &new_error {
                        tracing::error!(event="service_error",error=%message);
                        sink(UiEvent::Notify {
                            review: false,
                            id: format!("gopher-error-{}", hash(message)),
                            title: "Gopher needs attention".into(),
                            body: message.clone(),
                        });
                    } else {
                        tracing::info!(event = "service_recovered");
                    }
                }
                error = new_error;
                failures = if error.is_some() {
                    failures.saturating_add(1)
                } else {
                    0
                };
                let delay = config
                    .poll_seconds
                    .saturating_mul(2_u64.pow(failures.min(5)))
                    .min(900);
                deadline = Instant::now() + Duration::from_secs(delay);
                invalidated_poll_ids.clear();
                if rediscover_after_poll {
                    rediscover_after_poll = false;
                    discovered_at = None;
                    deadline = Instant::now();
                }
            }
        }
        if shutting_down {
            if !pr_actions.has_submissions() {
                // A LabelDone handler queues LabelSaved before this barrier.
                // Persist that result before dropping the runtime and instance lock.
                let _ = sender.send(Command::ShutdownComplete);
            }
            continue;
        }
        pr_actions.reconcile(&action_worker::Context {
            store: &store,
            prs: &prs,
            viewer: viewer.as_deref(),
            config: &config,
            sender: &sender,
            sink: &sink,
        });
        pr_actions.refresh_catalogues(
            &action_worker::Context {
                store: &store,
                prs: &prs,
                viewer: viewer.as_deref(),
                config: &config,
                sender: &sender,
                sink: &sink,
            },
            None,
        );
        sink(UiEvent::Updated {
            prs: prs.values().cloned().collect(),
            error: error.clone(),
            loading: polling,
        });
        if let Some(pr) = reveal_target {
            sink(UiEvent::ShowPopover { pr });
        }
    }
    tracing::info!(event = "worker_stopped");
    Ok(())
}

pub fn transition(
    snapshot: Snapshot,
    previous: Option<&PullRequest>,
    expected: Option<&[Agent]>,
    now: i64,
    settle_seconds: u64,
) -> PullRequest {
    let agents = reviewers::evaluate(&snapshot, previous, expected);
    let raw = reviewers::aggregate(&snapshot, &agents);
    let candidate_id = fingerprint(&snapshot, &agents, raw);
    let candidate_since = previous
        .filter(|p| !p.stale && p.candidate_id == candidate_id)
        .map_or(now, |p| p.candidate_since);
    let state = if raw.actionable() && now.saturating_sub(candidate_since) < settle_seconds as i64 {
        State::Reviewing
    } else {
        raw
    };
    let update_id = fingerprint(&snapshot, &agents, state);
    let head_since = previous
        .filter(|p| p.snapshot.head == snapshot.head)
        .map_or(now, |p| p.head_since);
    let reviewing_since = (state == State::Reviewing).then(|| {
        previous
            .filter(|p| p.state == State::Reviewing && p.snapshot.head == snapshot.head)
            .and_then(|p| p.reviewing_since)
            .unwrap_or(now)
    });
    PullRequest {
        snapshot,
        agents,
        state,
        update_id,
        acknowledged: previous.and_then(|p| p.acknowledged.clone()),
        fetched_at: now,
        stale: false,
        error: None,
        head_since,
        candidate_id,
        candidate_since,
        reviewing_since,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_flushes_label_saves_queued_after_the_quit_request() {
        for account in ["test", "old-account"] {
            let directory = tempfile::tempdir().unwrap();
            let mut store = Store::open(directory.path()).unwrap();
            store.set_viewer("test").unwrap();
            store
                .save(&transition(
                    Snapshot {
                        id: "PR_1".into(),
                        repo: "owner/repo".into(),
                        open: true,
                        ..Default::default()
                    },
                    None,
                    None,
                    100,
                    0,
                ))
                .unwrap();
            let (sender, receiver) = unbounded_channel();
            sender.send(Command::Shutdown).unwrap();
            sender.send(Command::Shutdown).unwrap(); // UI quit followed by Worker::drop.
            sender
                .send(Command::LabelSaved {
                    pr: "PR_1".into(),
                    viewer: account.into(),
                    label: PrLabel {
                        name: "saved".into(),
                        color: "112233".into(),
                    },
                    selected: true,
                })
                .unwrap();
            run(
                directory.path().into(),
                Config {
                    gh_path: Some(directory.path().join("missing-gh")),
                    ..Default::default()
                },
                receiver,
                sender,
                Arc::new(|_| {}),
                IGNORED_CHECK_INTERVAL,
            )
            .await
            .unwrap();
            let labels = store.load().unwrap()[0].snapshot.labels.clone();
            if account == "test" {
                assert_eq!(labels[0].name, "saved");
            } else {
                assert!(
                    labels.is_empty(),
                    "An old account must not change the current cache"
                );
            }
        }
    }

    fn next_list_event(rx: &std::sync::mpsc::Receiver<UiEvent>) -> UiEvent {
        loop {
            let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if !matches!(event, UiEvent::ActionsChanged(_)) {
                return event;
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn discovery_omissions_keep_polling_known_prs_only_for_the_same_account() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("gh");
        let script = include_str!("../tests/fixtures/gh-snapshot.sh").replace("input=$(cat)", r#"
input=$(cat)
case "$input" in
  *PollHeads*) echo '{"data":{"viewer":{"login":"test"},"nodes":[{"id":"PR_1","headRefOid":"head","state":"OPEN"}]}}'; exit 0 ;;
  *AuthoredPrs*) echo '{"data":{"viewer":{"pullRequests":{"pageInfo":{"hasNextPage":false},"nodes":[]}}}}'; exit 0 ;;
  *search*) echo '{"data":{"search":{"issueCount":0,"pageInfo":{"hasNextPage":false},"nodes":[]}}}'; exit 0 ;;
  *viewer*) echo '{"data":{"viewer":{"login":"test"}}}'; exit 0 ;;
esac
"#);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nif [ \"$1\" = auth ]; then echo test-credential; exit 0; fi\n{script}"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = Config {
            gh_path: Some(path.clone()),
            ..Default::default()
        };
        let references = || {
            vec![PrRef {
                id: "PR_1".into(),
                repo: "owner/repo".into(),
                number: 1,
            }]
        };
        let batch = fetch(
            config.clone(),
            references(),
            true,
            Some("test".into()),
            BTreeSet::new(),
        )
        .await
        .unwrap();
        assert_eq!(batch.references.len(), 1);
        assert!(batch.results[0].1.as_ref().unwrap().open);

        // A direct closed result still reaches the worker so it can remove the PR.
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nif [ \"$1\" = auth ]; then echo test-credential; exit 0; fi\n{}",
                script.replace("\"state\":\"OPEN\"", "\"state\":\"CLOSED\"")
            ),
        )
        .unwrap();
        let batch = fetch(
            config.clone(),
            references(),
            true,
            Some("test".into()),
            BTreeSet::new(),
        )
        .await
        .unwrap();
        assert!(!batch.results[0].1.as_ref().unwrap().open);

        for (viewer, ignored) in [
            ("different-account", BTreeSet::new()),
            ("test", BTreeSet::from(["PR_1".into()])),
        ] {
            let batch = fetch(
                config.clone(),
                references(),
                true,
                Some(viewer.into()),
                ignored,
            )
            .await
            .unwrap();
            assert!(batch.references.is_empty());
            assert!(batch.results.is_empty());
        }
    }

    #[test]
    fn ignored_pr_cannot_reappear_from_an_in_flight_poll() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config {
            gh_path: Some(directory.path().join("missing-gh")),
            ..Default::default()
        };
        let snapshot = Snapshot {
            id: "PR_ignored".into(),
            repo: "owner/repo".into(),
            number: 1,
            head: "abc".into(),
            open: true,
            ..Default::default()
        };
        let mut store = Store::open(directory.path()).unwrap();
        store.set_viewer("viewer").unwrap();
        let pr = transition(snapshot.clone(), None, None, 100, 0);
        store.save(&pr).unwrap();
        let notification = store.notification(&pr).unwrap().unwrap();
        drop(store);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = start(
            directory.path().into(),
            config,
            Arc::new(move |event| {
                tx.send(event).unwrap();
            }),
        )
        .unwrap();
        // Let initial polling fail, then inject a delayed discovery response deterministically.
        loop {
            if matches!(
                next_list_event(&rx),
                UiEvent::Updated { error: Some(_), .. }
            ) {
                break;
            }
        }
        worker
            .sender
            .send(Command::Ignore(snapshot.id.clone()))
            .unwrap();
        // An ignored-details completion can emit list events before the delayed
        // poll completes. Force that interleaving instead of relying on timing.
        worker
            .sender
            .send(Command::IgnoredDetailsLoaded(Ok(vec![snapshot.clone()])))
            .unwrap();
        worker
            .sender
            .send(Command::PollComplete(Ok(Batch {
                viewer: "viewer".into(),
                discovered: true,
                references: vec![PrRef {
                    id: snapshot.id.clone(),
                    repo: snapshot.repo.clone(),
                    number: 1,
                }],
                results: vec![(snapshot.id.clone(), Ok(snapshot.clone()))],
            })))
            .unwrap();
        worker
            .sender
            .send(Command::NotificationAction {
                id: notification.clone(),
                open: false,
                reveal: true,
            })
            .unwrap();
        // Commands above share a FIFO sender. ShowPopover is emitted after the
        // notification action's persisted UI update, so it marks completion of
        // the whole sequence without assuming one UI event per command.
        let mut ignored = false;
        let mut poll_completed = false;
        loop {
            let event = next_list_event(&rx);
            match event {
                UiEvent::DismissNotifications(ids) if ids == vec![notification.clone()] => {
                    ignored = true;
                }
                UiEvent::Updated { prs, error, .. } if ignored => {
                    assert!(prs.is_empty(), "Ignored PR reappeared: {prs:?}");
                    poll_completed |= error.is_none();
                }
                UiEvent::ShowPopover { pr } => {
                    assert!(pr.is_none(), "Ignored notification must not reveal a PR");
                    break;
                }
                UiEvent::Stopped => panic!("Worker stopped before completing the sequence"),
                UiEvent::Notify { review: true, .. } if ignored => {
                    panic!("Ignored PR generated a review notification");
                }
                _ => {}
            }
        }
        assert!(ignored, "Ignore must dismiss the saved notification");
        assert!(
            poll_completed,
            "The delayed poll must complete successfully"
        );
        drop(worker);
        let store = Store::open(directory.path()).unwrap();
        assert!(store.load().unwrap().is_empty());
        assert!(store.ignored().unwrap().contains(&snapshot.id));
    }

    #[cfg(unix)]
    #[test]
    fn restore_during_poll_discards_old_result_and_requests_discovery() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let gh = directory.path().join("gh");
        std::fs::write(
            &gh,
            "#!/bin/sh\nif [ \"$1\" = auth ]; then echo test-credential; exit 0; fi\ntouch \"$(dirname \"$0\")/started\"\nexec sleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = Config {
            gh_path: Some(gh),
            notifications: false,
            ..Default::default()
        };
        let snapshot = Snapshot {
            id: "PR_restore".into(),
            repo: "owner/repo".into(),
            number: 42,
            open: true,
            head: "current".into(),
            ..Default::default()
        };
        let store = Store::open(directory.path()).unwrap();
        store
            .save(&transition(snapshot.clone(), None, None, 100, 0))
            .unwrap();
        drop(store);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = start(
            directory.path().into(),
            config,
            Arc::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .unwrap();
        let started = Instant::now();
        while !directory.path().join("started").exists() {
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }
        // The real request is now in flight, so both commands invalidate that run.
        worker
            .sender
            .send(Command::Ignore(snapshot.id.clone()))
            .unwrap();
        worker
            .sender
            .send(Command::Restore(snapshot.id.clone()))
            .unwrap();
        loop {
            if let UiEvent::IgnoredUpdated { prs, .. } = next_list_event(&rx) {
                assert!(prs.is_empty());
                break;
            }
        }
        // Consume the Restore command's active-state event before injecting a result.
        assert!(matches!(next_list_event(&rx),UiEvent::Updated{prs,..} if prs.is_empty()));
        std::fs::remove_file(directory.path().join("started")).unwrap();
        let batch = || Batch {
            viewer: "viewer".into(),
            discovered: true,
            references: vec![PrRef {
                id: snapshot.id.clone(),
                repo: snapshot.repo.clone(),
                number: 42,
            }],
            results: vec![(snapshot.id.clone(), Ok(snapshot.clone()))],
        };
        worker
            .sender
            .send(Command::PollComplete(Ok(batch())))
            .unwrap();
        assert!(matches!(next_list_event(&rx),UiEvent::Updated{prs,..} if prs.is_empty()));
        // Restore overrides the normal polling delay even if the old request completed later.
        let started = Instant::now();
        while !directory.path().join("started").exists() {
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(
            next_list_event(&rx),
            UiEvent::Updated { loading: true, .. }
        ));
        worker
            .sender
            .send(Command::PollComplete(Ok(batch())))
            .unwrap();
        assert!(
            matches!(next_list_event(&rx),UiEvent::Updated{prs,loading:false,..} if prs.len()==1)
        );
        drop(worker);
        let store = Store::open(directory.path()).unwrap();
        assert!(!store.ignored().unwrap().contains(&snapshot.id));
        assert!(store.load_ignored().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ignored_checks_repeat_without_opening_the_view_or_refreshing_active_prs() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let gh = directory.path().join("gh");
        std::fs::write(&gh, r#"#!/bin/sh
if [ "$1" = auth ]; then echo test-credential; exit 0; fi
payload=$(cat)
case "$payload" in
 *IgnoredPrDetails*)
   marker="$(dirname "$0")/checked"
   if [ -f "$marker" ]; then state=OPEN; else state=CLOSED; touch "$marker"; fi
   echo "{\"data\":{\"nodes\":[{\"id\":\"ignored\",\"number\":42,\"title\":\"Test\",\"url\":\"https://github.com/owner/repo/pull/42\",\"state\":\"$state\",\"isDraft\":false,\"repository\":{\"nameWithOwner\":\"owner/repo\"}}]}}"
   ;;
 *AuthoredPrs*) echo '{"data":{"viewer":{"pullRequests":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}';;
 *viewer*) echo '{"data":{"viewer":{"login":"test"}}}';;
 *) echo '{"data":{"search":{"issueCount":0,"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}';;
esac
"#).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut store = Store::open(directory.path()).unwrap();
        let pr = PullRequest::unreviewed(Snapshot {
            id: "ignored".into(),
            number: 42,
            repo: "owner/repo".into(),
            open: true,
            ..Default::default()
        });
        store.save(&pr).unwrap();
        store.ignore(&pr.snapshot.id).unwrap();
        drop(store);
        let config = Config {
            gh_path: Some(gh),
            poll_seconds: 3600,
            notifications: false,
            ..Default::default()
        };
        let (sender, receiver) = unbounded_channel();
        let (ui_sender, mut ui_receiver) = unbounded_channel();
        let task = tokio::spawn(run(
            directory.path().into(),
            config,
            receiver,
            sender.clone(),
            Arc::new(move |event| {
                let _ = ui_sender.send(event);
            }),
            Duration::from_millis(100),
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut closed_seen = false;
            loop {
                if let UiEvent::IgnoredUpdated {
                    prs,
                    error,
                    loading: false,
                } = ui_receiver.recv().await.unwrap()
                {
                    assert!(error.is_none());
                    if !closed_seen {
                        assert!(prs.is_empty());
                        closed_seen = true;
                    } else {
                        assert_eq!(prs.len(), 1);
                        assert_eq!(prs[0].snapshot.id, "ignored");
                        break;
                    }
                }
            }
        })
        .await
        .unwrap();
        sender.send(Command::Shutdown).unwrap();
        task.await.unwrap().unwrap();
        assert!(
            Store::open(directory.path())
                .unwrap()
                .ignored()
                .unwrap()
                .contains("ignored")
        );
    }
}
mod action_worker;
pub use action_worker::ActionCommand;
