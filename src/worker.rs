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

#[derive(Clone, Debug)]
pub enum UiEvent {
    Updated {
        prs: Vec<PullRequest>,
        error: Option<String>,
    },
    Notify {
        id: String,
        title: String,
        body: String,
    },
    Open {
        url: String,
        pr: String,
        update: String,
    },
}
pub enum Command {
    Refresh,
    Acknowledge {
        pr: String,
        update: String,
        checked: bool,
    },
    NotificationClicked(String),
    NotificationDelivered(String),
    NotificationFailed(String),
    Shutdown,
    PollComplete(std::result::Result<Batch, String>),
}
pub struct Batch {
    viewer: String,
    references: Vec<PrRef>,
    discovered: bool,
    results: Vec<(String, std::result::Result<Snapshot, String>)>,
}
pub type Sink = Arc<dyn Fn(UiEvent) + Send + Sync>;

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
            let run = || -> Result<()> {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()?;
                runtime.block_on(run(directory, config, receiver, task_sender, sink.clone()))
            };
            if let Err(error) = run() {
                tracing::error!(event="worker_failed", error=%error);
                sink(UiEvent::Updated {
                    prs: vec![],
                    error: Some(error.to_string()),
                });
                sink(UiEvent::Notify {
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
) -> Result<Batch> {
    let github = Github::new(&config)?;
    let viewer = github.viewer().await?;
    let discovered = discover || old_viewer.as_deref() != Some(&viewer);
    let references = if discovered {
        github.discover(&viewer).await?
    } else {
        references
    };
    let references: Vec<_> = references
        .into_iter()
        .filter(|r| !config.repositories.get(&r.repo).is_some_and(|c| c.ignore))
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
                let result = github.snapshot(&reference).await.map_err(|e| e.to_string());
                (reference.id, result)
            });
        }
        let Some(result) = tasks.join_next().await else {
            break;
        };
        results.push(result?);
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
) -> Result<()> {
    let mut store = Store::open(&directory)?;
    let mut prs: BTreeMap<_, _> = store
        .load()?
        .into_iter()
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
    let mut viewer: Option<String> = None;
    let mut discovered_at: Option<Instant> = None;
    let mut deadline = Instant::now();
    let mut polling = false;
    let mut failures = 0_u32;
    let mut in_flight_notifications = BTreeSet::new();
    sink(UiEvent::Updated {
        prs: prs.values().cloned().collect(),
        error: None,
    });
    loop {
        let command = tokio::select! {
            command = receiver.recv() => match command { Some(c)=>c,None=>break },
            _ = tokio::time::sleep_until(deadline.into()), if !polling => {
                polling = true;
                let config = config.clone();
                let references = references.clone();
                let viewer = viewer.clone();
                let sender = sender.clone();
                let discover = discovered_at.is_none_or(|time|time.elapsed().as_secs() >= config.discovery_seconds);
                tokio::spawn(async move {
                    let result = fetch(config,references,discover,viewer).await.map_err(|e|e.to_string());
                    let _ = sender.send(Command::PollComplete(result));
                });
                continue;
            }
        };
        match command {
            Command::Shutdown => break,
            Command::Refresh => {
                deadline = Instant::now();
                discovered_at = None;
            }
            Command::Acknowledge {
                pr,
                update,
                checked,
            } => {
                if let Some(pr) = prs.get_mut(&pr) {
                    store.acknowledge(pr, &update, checked)?;
                }
            }
            Command::NotificationClicked(id) => {
                if let Some((pr, update, url)) = store.notification_target(&id)? {
                    sink(UiEvent::Open { url, pr, update });
                    tracing::info!(event="notification_clicked", notification=%id);
                }
            }
            Command::NotificationDelivered(id) => {
                store.mark_delivered(&id)?;
            }
            Command::NotificationFailed(id) => {
                in_flight_notifications.remove(&id);
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
                        references = batch.references;
                        if batch.discovered {
                            discovered_at = Some(Instant::now());
                            let ids: BTreeSet<_> =
                                references.iter().map(|r| r.id.clone()).collect();
                            prs.retain(|id, _| ids.contains(id));
                            store.retain(&ids)?;
                        }
                        let now = chrono::Utc::now().timestamp();
                        for (id, result) in batch.results {
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
            }
        }
        sink(UiEvent::Updated {
            prs: prs.values().cloned().collect(),
            error: error.clone(),
        });
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
    }
}
