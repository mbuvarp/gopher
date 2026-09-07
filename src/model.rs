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
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Unknown => "?",
            Self::Reviewing => "◌",
            Self::Comments => "●",
            Self::Approved => "✓",
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
}
impl PullRequest {
    pub fn needs_attention(&self) -> bool {
        !self.stale
            && self.state.actionable()
            && self.acknowledged.as_deref() != Some(&self.update_id)
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
