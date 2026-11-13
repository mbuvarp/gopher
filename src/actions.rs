//! Repository action preferences and worker-owned UI state.
use crate::model::{AgentResult, CheckProgress, CheckState, PullRequest, State};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Seconds Merge counts down before submitting; zero submits without a wait.
pub const DEFAULT_MERGE_DELAY_SECONDS: u8 = 5;
pub const MAX_MERGE_DELAY_SECONDS: u8 = 10;

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
    /// Label names offered as checkable items directly in each PR's Actions menu.
    pub quick_labels: BTreeSet<String>,
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
            quick_labels: BTreeSet::new(),
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
        match change {
            Setting::Enabled(kind, value) => match kind {
                Kind::Merge => self.merge.enabled = *value,
                Kind::Label => self.label.enabled = *value,
            },
            Setting::Condition(kind, value) => match kind {
                Kind::Merge => self.merge.condition = *value,
                Kind::Label => self.label.condition = *value,
            },
            Setting::MergeMethod(value) => self.merge_method = *value,
            Setting::QuickLabel(name, true) => {
                self.quick_labels.insert(name.clone());
            }
            Setting::QuickLabel(name, false) => {
                self.quick_labels.remove(name);
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum Setting {
    Enabled(Kind, bool),
    Condition(Kind, Condition),
    MergeMethod(MergeMethod),
    QuickLabel(String, bool),
}
#[derive(Clone, Debug)]
pub enum Request {
    ReadyForReview {
        pr: String,
        head: String,
        update: String,
    },
    Configure {
        repo: String,
        change: Setting,
    },
    Merge {
        pr: String,
        head: String,
        update: String,
        on_green: bool,
    },
    CancelMerge(String),
    /// Applies to countdowns started afterwards; running countdowns keep theirs.
    MergeDelay(u8),
    LoadLabels(String),
    Label {
        pr: String,
        name: String,
        selected: bool,
    },
    /// `update` is the displayed update ID, acknowledged once the save succeeds.
    SaveLabels {
        pr: String,
        name: Option<String>,
        update: String,
    },
    /// Toggles and immediately saves one label from the PR's Actions menu.
    QuickLabel {
        pr: String,
        name: String,
        selected: bool,
        update: String,
    },
    DismissError(DisplayedError),
    /// Refetch the PR and, only if the refetch reproduces what was displayed,
    /// write a diagnostic report for later investigation.
    Report {
        pr: String,
        displayed: Box<Displayed>,
    },
}
/// What the popover showed for a PR when the user reported it as incorrect.
/// A report is written only when a fresh fetch produces the same values, so a
/// merely stale display is refreshed instead of reported.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Displayed {
    pub head: String,
    pub update_id: String,
    pub candidate_id: String,
    pub state: State,
    pub check_state: Option<CheckState>,
    pub check_progress: Option<CheckProgress>,
    pub stale: bool,
    pub agents: Vec<AgentResult>,
}
impl Displayed {
    pub fn of(pr: &PullRequest) -> Self {
        Self {
            head: pr.snapshot.head.clone(),
            update_id: pr.update_id.clone(),
            candidate_id: pr.candidate_id.clone(),
            state: pr.state,
            check_state: pr.snapshot.check_state,
            check_progress: pr.snapshot.check_progress,
            stale: pr.stale,
            agents: pr.agents.clone(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReportProgress {
    Checking,
    Saved(Failure),
    /// The refetch differed from the display, so nothing was written.
    Changed(Failure),
    Failed(Failure),
}
impl ReportProgress {
    /// The dismissible message occurrence, if the report has finished.
    pub fn outcome(&self) -> Option<&Failure> {
        match self {
            Self::Checking => None,
            Self::Saved(outcome) | Self::Changed(outcome) | Self::Failed(outcome) => Some(outcome),
        }
    }
    pub fn text(&self) -> &str {
        match self {
            Self::Checking => "Refetching to confirm the report…",
            Self::Saved(outcome) | Self::Changed(outcome) | Self::Failed(outcome) => {
                &outcome.message
            }
        }
    }
}
/// An action error with a worker-unique ID, so identical repeated messages
/// remain distinct occurrences.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub id: u64,
    pub message: String,
}
/// The specific failure occurrence the popover displayed, so dismissing it
/// cannot clear a newer failure that arrived after the user saw the old one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisplayedError {
    Ready { pr: String, id: u64 },
    Merge { pr: String, id: u64 },
    Report { pr: String, id: u64 },
    Request(u64),
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
    pub unsaved: BTreeSet<String>,
    pub error: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeProgress {
    WaitingForChecks,
    Countdown(u8),
    /// Waiting for the numbered PR's merge into the same base branch to finish.
    Queued(u64),
    Checking,
    Merging,
    Complete,
    Failed(Failure),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadyProgress {
    Checking,
    Submitting,
    Failed(Failure),
}
impl MergeProgress {
    pub fn text(&self) -> String {
        match self {
            Self::WaitingForChecks => "Merge pending…".into(),
            Self::Countdown(seconds) => format!("Cancel ({seconds}s)"),
            Self::Queued(number) => format!("Waiting for #{number}…"),
            Self::Checking => "Checking merge…".into(),
            Self::Merging => "Merging…".into(),
            Self::Complete => "Merged".into(),
            Self::Failed(failure) => failure.message.clone(),
        }
    }
    pub fn busy(&self) -> bool {
        matches!(
            self,
            Self::WaitingForChecks
                | Self::Countdown(_)
                | Self::Queued(_)
                | Self::Checking
                | Self::Merging
        )
    }
}
#[derive(Clone, Debug)]
pub struct ActionState {
    pub preferences: BTreeMap<String, Preferences>,
    pub merges: BTreeMap<String, MergeProgress>,
    pub ready: BTreeMap<String, ReadyProgress>,
    pub labels: BTreeMap<String, Labels>,
    pub reports: BTreeMap<String, ReportProgress>,
    pub error: Option<Failure>,
    pub merge_delay: u8,
}
impl Default for ActionState {
    fn default() -> Self {
        Self {
            preferences: BTreeMap::new(),
            merges: BTreeMap::new(),
            ready: BTreeMap::new(),
            labels: BTreeMap::new(),
            reports: BTreeMap::new(),
            error: None,
            merge_delay: DEFAULT_MERGE_DELAY_SECONDS,
        }
    }
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
    /// A Ready for review request is validating, submitting, or awaiting its
    /// saved result; the PR is displayed optimistically as ready meanwhile.
    pub fn ready_pending(&self, pr: &str) -> bool {
        matches!(
            self.ready.get(pr),
            Some(ReadyProgress::Checking | ReadyProgress::Submitting)
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LabelCatalogue {
    pub labels: Vec<crate::model::PrLabel>,
    pub fetched_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_saved_before_quick_labels_still_load() {
        let preferences: Preferences = serde_json::from_str(
            r#"{"merge":{"enabled":true,"condition":"approved"},"label":{"enabled":true,"condition":"always"},"merge_method":"squash"}"#,
        )
        .unwrap();
        assert!(preferences.quick_labels.is_empty());
        assert!(preferences.label.enabled);
    }

    #[test]
    fn quick_label_settings_toggle_names() {
        let mut preferences = Preferences::default();
        preferences.apply(&Setting::QuickLabel("bug".into(), true));
        preferences.apply(&Setting::QuickLabel("ready".into(), true));
        preferences.apply(&Setting::QuickLabel("bug".into(), false));
        assert_eq!(
            preferences.quick_labels.into_iter().collect::<Vec<_>>(),
            ["ready"]
        );
    }
}
