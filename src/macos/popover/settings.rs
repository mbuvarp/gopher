use super::*;
use crate::hotkeys::HotkeyAction;
use crate::macos::keyboard::{Edit, State as KeyboardState};

struct ShortcutRow {
    action: HotkeyAction,
    record: Retained<NSButton>,
    clear: Retained<NSButton>,
}
pub(super) struct SettingsEditor {
    pub view: Retained<FlippedView>,
    rows: Vec<ShortcutRow>,
    reset: Retained<NSButton>,
    file: Retained<NSButton>,
    message: Retained<NSTextField>,
    checks: Retained<NSButton>,
    downloads: Retained<NSButton>,
    updates_message: Retained<NSTextField>,
}
impl SettingsEditor {
    pub fn new(target: &ActionTarget) -> Self {
        let mtm = MainThreadMarker::new().unwrap();
        let view = FlippedView::new(rect(0.0, 0.0, WIDTH, 660.0), mtm);
        let heading = label("Hotkeys", 16.0, false, mtm);
        heading.setFont(Some(&NSFont::boldSystemFontOfSize(16.0)));
        heading.setFrame(rect(20.0, 10.0, 200.0, 24.0));
        view.addSubview(&heading);
        let hint = label(
            "Click a shortcut, then press its replacement. Escape cancels.\nOpen Gopher works globally; other shortcuts work in the inbox.",
            11.0,
            true,
            mtm,
        );
        hint.setFrame(rect(20.0, 40.0, WIDTH - 40.0, 36.0));
        view.addSubview(&hint);
        let mut rows = Vec::new();
        for (index, action) in HotkeyAction::ALL.into_iter().enumerate() {
            let y = 88.0 + index as f64 * 36.0;
            let name = label(action.label(), 13.0, false, mtm);
            name.setFrame(rect(20.0, y + 4.0, 220.0, 24.0));
            view.addSubview(&name);
            let record = target.button("", AppEvent::HotkeyEdit(Edit::Record(action)), mtm);
            record.setFrame(rect(256.0, y, 208.0, 28.0));
            view.addSubview(&record);
            let clear = target.button("Clear", AppEvent::HotkeyEdit(Edit::Clear(action)), mtm);
            clear.setFrame(rect(476.0, y, 80.0, 28.0));
            view.addSubview(&clear);
            rows.push(ShortcutRow {
                action,
                record,
                clear,
            });
        }
        let reset = target.button("Restore defaults", AppEvent::HotkeyEdit(Edit::Reset), mtm);
        reset.setFrame(rect(20.0, 392.0, 136.0, 28.0));
        view.addSubview(&reset);
        let file = target.button(
            "Open configuration file",
            AppEvent::PopoverAction(Action::ConfigFile),
            mtm,
        );
        file.setFrame(rect(174.0, 392.0, 190.0, 28.0));
        file.setToolTip(Some(&NSString::from_str(
            "Advanced config.toml changes still require a restart.",
        )));
        view.addSubview(&file);
        let message = label("", 11.0, true, mtm);
        message.setFrame(rect(20.0, 434.0, WIDTH - 40.0, 40.0));
        view.addSubview(&message);
        let font_size = if crate::identity::IS_DEV { 13.0 } else { 16.0 };
        let heading = label(&crate::identity::version_label(), font_size, false, mtm);
        heading.setFont(Some(&NSFont::boldSystemFontOfSize(font_size)));
        heading.setFrame(if crate::identity::IS_DEV {
            rect(20.0, 480.0, WIDTH - 40.0, 40.0)
        } else {
            rect(20.0, 490.0, WIDTH - 40.0, 24.0)
        });
        view.addSubview(&heading);
        let checks = target.button(
            "Automatically check for updates daily",
            AppEvent::UpdateEdit(super::super::updates::Edit::Checks),
            mtm,
        );
        let downloads = target.button(
            "Download updates and install when Gopher quits",
            AppEvent::UpdateEdit(super::super::updates::Edit::Downloads),
            mtm,
        );
        for (index, button) in [&checks, &downloads].into_iter().enumerate() {
            button.setButtonType(NSButtonType::Switch);
            button.setFrame(rect(20.0, 525.0 + index as f64 * 30.0, WIDTH - 40.0, 26.0));
            view.addSubview(button);
        }
        let updates_message = label("", 11.0, true, mtm);
        updates_message.setFrame(rect(20.0, 592.0, WIDTH - 40.0, 52.0));
        view.addSubview(&updates_message);
        Self {
            view,
            rows,
            reset,
            file,
            message,
            checks,
            downloads,
            updates_message,
        }
    }
    pub fn update(&self, state: &KeyboardState, updates: &super::super::updates::State) {
        self.checks.setEnabled(updates.enabled);
        self.downloads.setEnabled(updates.enabled && updates.checks);
        self.checks.setState(if updates.checks { 1 } else { 0 });
        self.downloads
            .setState(if updates.downloads { 1 } else { 0 });
        set_text(
            &self.updates_message,
            if updates.message.is_empty() {
                "Updates install on quit. Use Actions to check or install and relaunch now."
            } else {
                &updates.message
            },
        );
        for row in &self.rows {
            let binding = state.preferences.binding(row.action);
            let title = if state.recording == Some(row.action) {
                "Press shortcut…".into()
            } else {
                binding
                    .as_ref()
                    .map(|b| b.display())
                    .unwrap_or_else(|| "Not set".into())
            };
            row.record.setTitle(&NSString::from_str(&title));
            row.record.setEnabled(state.ready && !state.pending);
            row.clear
                .setEnabled(state.ready && !state.pending && binding.is_some());
        }
        self.reset.setEnabled(state.ready && !state.pending);
        let message = state.error.as_deref().unwrap_or(if state.pending {
            "Saving…"
        } else if !state.ready {
            "Loading settings…"
        } else {
            "Changes save and apply automatically."
        });
        set_text(&self.message, message);
        let color = if state.error.is_some() {
            NSColor::systemRedColor()
        } else {
            NSColor::secondaryLabelColor()
        };
        self.message.setTextColor(Some(&color));
    }
    pub fn remove(self, target: &ActionTarget) {
        for row in self.rows {
            for button in [&row.record, &row.clear] {
                target.ivars().actions.borrow_mut().remove(&button.tag());
            }
        }
        for button in [&self.reset, &self.file, &self.checks, &self.downloads] {
            target.ivars().actions.borrow_mut().remove(&button.tag());
        }
    }
}
