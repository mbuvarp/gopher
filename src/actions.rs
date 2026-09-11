//! Repository action preferences and worker-owned UI state.
use crate::model::{CheckState, PullRequest, State};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Merge,
    Label,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    #[default]
    Always,
    Reviewing,
    Comments,
    Approved,
}
impl Condition {
    pub const ALL: [Self; 4] = [
        Self::Always,
        Self::Reviewing,
        Self::Comments,
        Self::Approved,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Always => "Always",
            Self::Reviewing => "Reviewing",
            Self::Comments => "Comments",
            Self::Approved => "Approved",
        }
    }
    pub fn matches(self, state: State) -> bool {
        match self {
            Self::Always => true,
            Self::Reviewing => state == State::Reviewing,
            Self::Comments => state == State::Comments,
            Self::Approved => state == State::Approved,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergeMethod {
    #[default]
    Merge,
    Squash,
    Rebase,
}
impl MergeMethod {
    pub const ALL: [Self; 3] = [Self::Merge, Self::Squash, Self::Rebase];
    pub fn label(self) -> &'static str {
        match self {
            Self::Merge => "Merge commit",
            Self::Squash => "Squash",
            Self::Rebase => "Rebase",
        }
    }
    pub fn api_name(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Squash => "squash",
            Self::Rebase => "rebase",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rule {
    pub enabled: bool,
    pub condition: Condition,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Preferences {
    pub merge: Rule,
    pub label: Rule,
    pub merge_method: MergeMethod,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            merge: Rule {
                enabled: false,
                condition: Condition::Approved,
            },
            label: Rule::default(),
            merge_method: MergeMethod::Merge,
        }
    }
}
impl Preferences {
    pub fn rule(&self, kind: Kind) -> &Rule {
        match kind {
            Kind::Merge => &self.merge,
            Kind::Label => &self.label,
        }
    }
    pub fn allows(&self, kind: Kind, pr: &PullRequest) -> bool {
        let rule = self.rule(kind);
        rule.enabled
            && !pr.stale
            && pr.snapshot.open
            && rule.condition.matches(pr.state)
            && (kind != Kind::Merge
                || (!pr.snapshot.draft && pr.snapshot.check_state != Some(CheckState::Conflicts)))
    }
    pub fn apply(&mut self, change: &Setting) {
        match *change {
            Setting::Enabled(kind, value) => match kind {
                Kind::Merge => self.merge.enabled = value,
                Kind::Label => self.label.enabled = value,
            },
            Setting::Condition(kind, value) => match kind {
                Kind::Merge => self.merge.condition = value,
                Kind::Label => self.label.condition = value,
            },
            Setting::MergeMethod(value) => self.merge_method = value,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Setting {
    Enabled(Kind, bool),
    Condition(Kind, Condition),
    MergeMethod(MergeMethod),
}
#[derive(Clone, Debug)]
pub enum Request {
    Configure {
        repo: String,
        change: Setting,
    },
    Merge {
        pr: String,
        head: String,
        update: String,
    },
    CancelMerge(String),
    LoadLabels(String),
    Label {
        pr: String,
        name: String,
        selected: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    pub color: String,
    pub selected: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Labels {
    pub items: Vec<Label>,
    pub loading: bool,
    pub catalogue_ready: bool,
    pub catalogue_error: Option<String>,
    pub pending: BTreeSet<String>,
    pub error: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeProgress {
    Countdown(u8),
    Checking,
    Merging,
    Complete,
    Failed(String),
}
impl MergeProgress {
    pub fn text(&self) -> String {
        match self {
            Self::Countdown(seconds) => format!("Cancel ({seconds}s)"),
            Self::Checking => "Checking merge…".into(),
            Self::Merging => "Merging…".into(),
            Self::Complete => "Merged".into(),
            Self::Failed(error) => error.clone(),
        }
    }
    pub fn busy(&self) -> bool {
        matches!(self, Self::Countdown(_) | Self::Checking | Self::Merging)
    }
}
#[derive(Clone, Debug, Default)]
pub struct ActionState {
    pub preferences: BTreeMap<String, Preferences>,
    pub merges: BTreeMap<String, MergeProgress>,
    pub labels: BTreeMap<String, Labels>,
    pub error: Option<String>,
}
pub fn repo_key(repo: &str) -> String {
    repo.to_ascii_lowercase()
}
impl ActionState {
    pub fn preferences(&self, repo: &str) -> Preferences {
        self.preferences
            .get(&repo_key(repo))
            .cloned()
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LabelCatalogue {
    pub labels: Vec<crate::model::PrLabel>,
    pub fetched_at: i64,
}
