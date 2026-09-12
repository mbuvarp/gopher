//! Persisted shortcut preferences and transient inbox navigation.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const COMMAND: u8 = 1;
pub const OPTION: u8 = 2;
pub const CONTROL: u8 = 4;
pub const SHIFT: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyAction {
    OpenGopher,
    Next,
    Previous,
    Acknowledge,
    OpenPr,
    Details,
    Actions,
    Refresh,
}
impl HotkeyAction {
    pub const ALL: [Self; 8] = [
        Self::OpenGopher,
        Self::Next,
        Self::Previous,
        Self::Acknowledge,
        Self::OpenPr,
        Self::Details,
        Self::Actions,
        Self::Refresh,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenGopher => crate::identity::OPEN,
            Self::Next => "Next PR",
            Self::Previous => "Previous PR",
            Self::Acknowledge => "Acknowledge",
            Self::OpenPr => "Open PR",
            Self::Details => "Toggle Details",
            Self::Actions => "Open Actions",
            Self::Refresh => "Refresh",
        }
    }
    fn default_binding(self) -> Option<Binding> {
        let (key, label) = match self {
            Self::OpenGopher => return None,
            Self::Next => (38, "J"),
            Self::Previous => (40, "K"),
            Self::Acknowledge => (49, "Space"),
            Self::OpenPr => (31, "O"),
            Self::Details => (2, "D"),
            Self::Actions => (0, "A"),
            Self::Refresh => (15, "R"),
        };
        Some(Binding {
            key,
            modifiers: 0,
            label: label.into(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    /// macOS virtual key code; bindings follow the recorded key across layouts.
    pub key: u16,
    pub modifiers: u8,
    pub label: String,
}
impl Binding {
    pub fn display(&self) -> String {
        let mut text = String::new();
        for (mask, symbol) in [(CONTROL, "⌃"), (OPTION, "⌥"), (SHIFT, "⇧"), (COMMAND, "⌘")]
        {
            if self.modifiers & mask != 0 {
                text.push_str(symbol);
            }
        }
        text.push_str(&self.label);
        text
    }
    pub fn matches(&self, key: u16, modifiers: u8) -> bool {
        self.key == key && self.modifiers == modifiers
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Preferences {
    bindings: BTreeMap<HotkeyAction, Option<Binding>>,
}
impl Preferences {
    pub fn binding(&self, action: HotkeyAction) -> Option<Binding> {
        self.bindings
            .get(&action)
            .cloned()
            .unwrap_or_else(|| action.default_binding())
    }
    pub fn changed(&self, action: HotkeyAction, binding: Option<Binding>) -> Result<Self> {
        let mut next = self.clone();
        next.bindings.insert(action, binding);
        next.validate()?;
        Ok(next)
    }
    pub fn validate(&self) -> Result<()> {
        let mut used = BTreeMap::new();
        for action in HotkeyAction::ALL {
            if let Some(binding) = self.binding(action) {
                ensure!(
                    key_code_name(binding.key).is_some(),
                    "This key is not supported as a shortcut"
                );
                ensure!(
                    binding.modifiers & !15 == 0,
                    "Unsupported shortcut modifier"
                );
                ensure!(
                    !is_settings_shortcut(binding.key, binding.modifiers),
                    "Command+, is reserved for Settings"
                );
                ensure!(
                    !binding.label.is_empty()
                        && binding.label.len() <= 32
                        && !binding.label.chars().any(char::is_control),
                    "Invalid shortcut label"
                );
                if action == HotkeyAction::OpenGopher {
                    ensure!(
                        binding.modifiers & (COMMAND | OPTION | CONTROL) != 0,
                        "Open Gopher needs Command, Option, or Control to avoid capturing ordinary typing"
                    );
                }
                if let Some(other) = used.insert((binding.key, binding.modifiers), action) {
                    anyhow::bail!(
                        "{} is already assigned to {}",
                        binding.display(),
                        other.label()
                    );
                }
            }
        }
        Ok(())
    }
    pub fn action(&self, key: u16, modifiers: u8) -> Option<HotkeyAction> {
        HotkeyAction::ALL.into_iter().find(|action| {
            self.binding(*action)
                .is_some_and(|b| b.matches(key, modifiers))
        })
    }
}

pub fn is_settings_shortcut(key: u16, modifiers: u8) -> bool {
    key == 43 && modifiers == COMMAND
}

/// Shared whitelist for the recorder and global-hotkey's keyboard-types mapping.
pub fn key_code_name(key: u16) -> Option<&'static str> {
    Some(match key {
        0 => "KeyA",
        1 => "KeyS",
        2 => "KeyD",
        3 => "KeyF",
        4 => "KeyH",
        5 => "KeyG",
        6 => "KeyZ",
        7 => "KeyX",
        8 => "KeyC",
        9 => "KeyV",
        11 => "KeyB",
        12 => "KeyQ",
        13 => "KeyW",
        14 => "KeyE",
        15 => "KeyR",
        16 => "KeyY",
        17 => "KeyT",
        18 => "Digit1",
        19 => "Digit2",
        20 => "Digit3",
        21 => "Digit4",
        22 => "Digit6",
        23 => "Digit5",
        24 => "Equal",
        25 => "Digit9",
        26 => "Digit7",
        27 => "Minus",
        28 => "Digit8",
        29 => "Digit0",
        30 => "BracketRight",
        31 => "KeyO",
        32 => "KeyU",
        33 => "BracketLeft",
        34 => "KeyI",
        35 => "KeyP",
        36 => "Enter",
        37 => "KeyL",
        38 => "KeyJ",
        39 => "Quote",
        40 => "KeyK",
        41 => "Semicolon",
        42 => "Backslash",
        43 => "Comma",
        44 => "Slash",
        45 => "KeyN",
        46 => "KeyM",
        47 => "Period",
        49 => "Space",
        50 => "Backquote",
        96 => "F5",
        97 => "F6",
        98 => "F7",
        99 => "F3",
        100 => "F8",
        101 => "F9",
        103 => "F11",
        109 => "F10",
        111 => "F12",
        118 => "F4",
        120 => "F2",
        122 => "F1",
        123 => "ArrowLeft",
        124 => "ArrowRight",
        125 => "ArrowDown",
        126 => "ArrowUp",
        _ => return None,
    })
}

#[derive(Default, Debug)]
pub struct Navigation {
    pub selected: Option<String>,
    pub order: Vec<String>,
}
impl Navigation {
    pub fn update(&mut self, order: Vec<String>) {
        self.order = order;
        if self
            .selected
            .as_ref()
            .is_some_and(|id| !self.order.contains(id))
        {
            self.selected = None;
        }
    }
    pub fn step(&mut self, forward: bool) {
        let index = self
            .selected
            .as_ref()
            .and_then(|id| self.order.iter().position(|p| p == id));
        self.selected = match (index, forward) {
            (None, true) => self.order.first(),
            (None, false) => self.order.last(),
            (Some(i), true) => self.order.get(i + 1),
            (Some(i), false) => i.checked_sub(1).and_then(|i| self.order.get(i)),
        }
        .cloned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_shortcut_is_reserved_for_every_configurable_action() {
        let binding = Binding {
            key: 43,
            modifiers: COMMAND,
            label: ",".into(),
        };
        for action in HotkeyAction::ALL {
            let error = Preferences::default()
                .changed(action, Some(binding.clone()))
                .unwrap_err();
            assert_eq!(error.to_string(), "Command+, is reserved for Settings");
        }
        assert!(is_settings_shortcut(43, COMMAND));
        assert!(!is_settings_shortcut(43, COMMAND | SHIFT));
        assert!(!is_settings_shortcut(43, 0));
    }
    #[test]
    fn defaults_conflicts_and_unsetting() {
        let prefs = Preferences::default();
        prefs.validate().unwrap();
        assert_eq!(prefs.action(38, 0), Some(HotkeyAction::Next));
        assert_eq!(prefs.action(38, SHIFT), None);
        assert!(prefs.binding(HotkeyAction::OpenGopher).is_none());
        assert!(
            prefs
                .changed(HotkeyAction::Previous, prefs.binding(HotkeyAction::Next))
                .is_err()
        );
        assert!(
            prefs
                .changed(HotkeyAction::OpenGopher, prefs.binding(HotkeyAction::Next))
                .is_err()
        );
        let cleared = prefs.changed(HotkeyAction::Next, None).unwrap();
        let restored: Preferences =
            serde_json::from_str(&serde_json::to_string(&cleared).unwrap()).unwrap();
        assert!(restored.binding(HotkeyAction::Next).is_none());
        assert_eq!(
            restored.binding(HotkeyAction::Previous),
            prefs.binding(HotkeyAction::Previous)
        );
    }
    #[test]
    fn navigation_has_none_at_both_ends_and_keeps_identity() {
        let mut nav = Navigation::default();
        nav.step(true);
        assert_eq!(nav.selected, None);
        nav.update(vec!["a".into(), "b".into()]);
        for expected in [Some("a"), Some("b"), None] {
            nav.step(true);
            assert_eq!(nav.selected.as_deref(), expected);
        }
        for expected in [Some("b"), Some("a"), None] {
            nav.step(false);
            assert_eq!(nav.selected.as_deref(), expected);
        }
        nav.step(true);
        nav.update(vec!["b".into(), "a".into()]);
        assert_eq!(nav.selected.as_deref(), Some("a"));
        nav.update(vec!["b".into()]);
        assert_eq!(nav.selected, None);
    }
}
