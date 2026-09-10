use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Codex,
    Cubic,
    CodeRabbit,
}

impl Agent {
    pub const ALL: [Self; 3] = [Self::Codex, Self::Cubic, Self::CodeRabbit];
    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Cubic => "Cubic",
            Self::CodeRabbit => "CodeRabbit",
        }
    }
    pub fn from_login(login: &str) -> Option<Self> {
        match login.trim_end_matches("[bot]") {
            "chatgpt-codex-connector" => Some(Self::Codex),
            "cubic-dev-ai" => Some(Self::Cubic),
            "coderabbitai" => Some(Self::CodeRabbit),
            _ => None,
        }
    }
    pub fn from_app(slug: &str) -> Option<Self> {
        match slug {
            "chatgpt-codex-connector" => Some(Self::Codex),
            "cubic-dev-ai" => Some(Self::Cubic),
            "coderabbitai" => Some(Self::CodeRabbit),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Unknown,
    Reviewing,
    Comments,
    Approved,
}
impl State {
    pub fn actionable(self) -> bool {
        matches!(self, Self::Comments | Self::Approved)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "Unknown",
            Self::Reviewing => "Reviewing",
            Self::Comments => "Comments",
            Self::Approved => "Approved",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Unknown,
    Running,
    Clean,
    Findings,
    Skipped,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AgentResult {
    pub agent: Agent,
    pub verdict: Verdict,
    pub run_id: String,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Review {
    pub id: String,
    pub author: String,
    pub body: String,
    pub state: String,
    pub commit: String,
    pub submitted_at: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Comment {
    pub id: String,
    pub author: String,
    pub body: String,
    pub updated_at: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Reaction {
    pub id: String,
    pub author: String,
    pub content: String,
    pub created_at: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Thread {
    pub id: String,
    pub resolved: bool,
    pub author: String,
    pub last_comment_id: String,
    pub updated_at: String,
    pub body_hash: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Check {
    pub id: String,
    pub app: String,
    pub name: String,
    pub status: String,
    pub conclusion: String,
    pub started_at: String,
    pub completed_at: String,
    pub summary: String,
}
/// Aggregate check severity, independent of agent review verdicts.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Default)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    #[default]
    Green,
    Running,
    Failed,
}
impl CheckState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "Checks running...",
            Self::Failed => "Checks failed",
            Self::Green => "Checks green",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrLabel {
    pub name: String,
    pub color: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Snapshot {
    pub id: String,
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub head: String,
    pub open: bool,
    pub draft: bool,
    pub reviews: Vec<Review>,
    pub comments: Vec<Comment>,
    pub reactions: Vec<Reaction>,
    pub threads: Vec<Thread>,
    pub checks: Vec<Check>,
    /// None for older caches or identity-only snapshots, until a full fetch succeeds.
    #[serde(default)]
    pub check_state: Option<CheckState>,
    #[serde(default)]
    pub labels: Vec<PrLabel>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PullRequest {
    pub snapshot: Snapshot,
    pub agents: Vec<AgentResult>,
    pub state: State,
    pub update_id: String,
    pub acknowledged: Option<String>,
    pub fetched_at: i64,
    pub stale: bool,
    pub error: Option<String>,
    pub head_since: i64,
    pub candidate_id: String,
    pub candidate_since: i64,
    /// Local observation time for the current continuous Reviewing state.
    #[serde(default)]
    pub reviewing_since: Option<i64>,
}
impl PullRequest {
    /// Identity-only data must never be treated as current review evidence.
    pub fn unreviewed(snapshot: Snapshot) -> Self {
        Self {
            snapshot,
            agents: vec![],
            state: State::Unknown,
            update_id: String::new(),
            acknowledged: None,
            fetched_at: 0,
            stale: true,
            error: None,
            head_since: 0,
            candidate_id: String::new(),
            candidate_since: 0,
            reviewing_since: None,
        }
    }
    pub fn needs_attention(&self) -> bool {
        !self.stale
            && self.state.actionable()
            && self.acknowledged.as_deref() != Some(&self.update_id)
    }

    pub fn status_label(&self, now: i64) -> String {
        if self.stale {
            return State::Unknown.label().into();
        }
        if self.state == State::Reviewing
            && let Some(started) = self.reviewing_since
        {
            let minutes = now.saturating_sub(started).max(0) / 60;
            return if minutes < 60 {
                format!("Reviewing ({minutes}m)")
            } else {
                format!("Reviewing ({}h {}m)", minutes / 60, minutes % 60)
            };
        }
        self.state.label().into()
    }
}

pub fn hash(value: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(value.as_ref()))
}

pub fn fingerprint(snapshot: &Snapshot, agents: &[AgentResult], state: State) -> String {
    let mut threads: Vec<_> = snapshot
        .threads
        .iter()
        .filter(|t| !t.resolved)
        .map(|t| (&t.id, &t.last_comment_id, &t.updated_at, &t.body_hash))
        .collect();
    threads.sort();
    let runs: Vec<_> = agents
        .iter()
        .map(|a| (a.agent, a.verdict, &a.run_id))
        .collect();
    hash(
        serde_json::to_vec(&(&snapshot.head, state, runs, threads))
            .expect("serializable fingerprint"),
    )
}
