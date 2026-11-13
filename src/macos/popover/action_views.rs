use super::*;
use crate::actions::{Condition, Displayed, Kind, MergeMethod, MergeProgress, ReportProgress};
use objc2::runtime::ProtocolObject;
use std::cell::{Cell, RefCell};

struct TrackingState {
    tracking: Cell<bool>,
    proxy: EventLoopProxy<AppEvent>,
}
define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = TrackingState]
    struct MenuTracker;
    unsafe impl NSObjectProtocol for MenuTracker {}
    unsafe impl NSMenuDelegate for MenuTracker {
        #[unsafe(method(menuWillOpen:))]
        fn will_open(&self, _menu: &NSMenu) {
            self.ivars().tracking.set(true);
        }
        #[unsafe(method(menuDidClose:))]
        fn did_close(&self, _menu: &NSMenu) {
            self.ivars().tracking.set(false);
            let _ = self.ivars().proxy.send_event(AppEvent::RefreshPopover);
        }
    }
);
impl MenuTracker {
    fn new(target: &ActionTarget, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TrackingState {
            tracking: Cell::new(false),
            proxy: target.ivars().proxy.clone(),
        });
        unsafe { msg_send![super(this), init] }
    }
}

fn menu_item(title: &str, event: Option<AppEvent>, target: &ActionTarget) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(MainThreadMarker::new().unwrap()),
            &NSString::from_str(title),
            event.as_ref().map(|_| sel!(activate:)),
            &NSString::from_str(""),
        )
    };
    if let Some(event) = event {
        item.setTag(target.register(event));
        unsafe {
            item.setTarget(Some(target));
        }
    }
    item
}
fn bind_item(item: &NSMenuItem, event: AppEvent, target: &ActionTarget) {
    target
        .ivars()
        .actions
        .borrow_mut()
        .insert(item.tag(), event);
}
fn forget(tags: impl IntoIterator<Item = isize>, target: &ActionTarget) {
    let mut events = target.ivars().actions.borrow_mut();
    for tag in tags {
        events.remove(&tag);
    }
}

const REPORT: &str = "Report as incorrect";

pub(super) struct PrActions {
    pub button: Retained<NSPopUpButton>,
    pub cancel: Retained<NSButton>,
    configure: Retained<NSMenuItem>,
    ready: Retained<NSMenuItem>,
    merge: Retained<NSMenuItem>,
    labels: Retained<NSMenuItem>,
    /// Repository quick labels, shown in catalogue order directly above Labels.
    quick: RefCell<Vec<(String, Retained<NSMenuItem>)>>,
    menu: Retained<NSMenu>,
    separator: Retained<NSMenuItem>,
    report: Retained<NSMenuItem>,
    ignore: Retained<NSMenuItem>,
    tracker: Retained<MenuTracker>,
}
fn report_request(pr: &PullRequest) -> AppEvent {
    AppEvent::PrAction(Request::Report {
        pr: pr.snapshot.id.clone(),
        displayed: Box::new(Displayed::of(pr)),
    })
}
/// Requests the opposite of the checkmark the user saw on the menu item.
fn quick_label_request(pr: &PullRequest, label: &crate::actions::Label) -> AppEvent {
    AppEvent::PrAction(Request::QuickLabel {
        pr: pr.snapshot.id.clone(),
        name: label.name.clone(),
        selected: !label.selected,
        update: pr.update_id.clone(),
    })
}
impl PrActions {
    pub fn new(pr: &PullRequest, target: &ActionTarget, mtm: MainThreadMarker) -> Self {
        let button = NSPopUpButton::initWithFrame_pullsDown(
            NSPopUpButton::alloc(mtm),
            rect(CONTENT_WIDTH - 104.0, 57.0, 104.0, 26.0),
            true,
        );
        button.setFont(Some(&NSFont::systemFontOfSize(12.0)));
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        let tracker = MenuTracker::new(target, mtm);
        menu.setDelegate(Some(ProtocolObject::from_ref(&*tracker)));
        menu.addItem(&menu_item("Actions", None, target));
        let configure = menu_item(
            "Configure",
            Some(AppEvent::PopoverAction(Action::ConfigureRepo(
                pr.snapshot.repo.clone(),
            ))),
            target,
        );
        let merge = menu_item(
            "Merge",
            Some(AppEvent::PrAction(Request::Merge {
                on_green: pr.snapshot.check_state == Some(crate::model::CheckState::Running),
                pr: pr.snapshot.id.clone(),
                head: pr.snapshot.head.clone(),
                update: pr.update_id.clone(),
            })),
            target,
        );
        let ready = menu_item(
            "Ready for review",
            Some(AppEvent::PrAction(Request::ReadyForReview {
                pr: pr.snapshot.id.clone(),
                head: pr.snapshot.head.clone(),
                update: pr.update_id.clone(),
            })),
            target,
        );
        let labels = menu_item(
            "Labels",
            Some(AppEvent::PopoverAction(Action::Labels {
                pr: pr.snapshot.id.clone(),
                update: pr.update_id.clone(),
            })),
            target,
        );
        let report = menu_item(REPORT, Some(report_request(pr)), target);
        let ignore = menu_item(
            "Ignore",
            Some(AppEvent::PopoverAction(Action::Ignore(
                pr.snapshot.id.clone(),
            ))),
            target,
        );
        let separator = NSMenuItem::separatorItem(mtm);
        menu.addItem(&configure);
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&ready);
        menu.addItem(&merge);
        menu.addItem(&labels);
        menu.addItem(&separator);
        menu.addItem(&report);
        menu.addItem(&ignore);
        button.setMenu(Some(&menu));
        let cancel = target.button(
            "Cancel (5s)",
            AppEvent::PrAction(Request::CancelMerge(pr.snapshot.id.clone())),
            mtm,
        );
        cancel.setFrame(rect(CONTENT_WIDTH - 124.0, 57.0, 124.0, 26.0));
        cancel.setHidden(true);
        Self {
            button,
            cancel,
            configure,
            ready,
            merge,
            labels,
            quick: RefCell::new(Vec::new()),
            menu,
            separator,
            report,
            ignore,
            tracker,
        }
    }
    /// Rebuilds the quick label items only when their names change, then
    /// refreshes each item's checkmark and the toggle its click requests.
    fn update_quick_labels(
        &self,
        pr: &PullRequest,
        state: &ActionState,
        allowed: bool,
        target: &ActionTarget,
    ) {
        let preferences = state.preferences(&pr.snapshot.repo);
        let labels = state.labels.get(&pr.snapshot.id);
        let wanted: Vec<&crate::actions::Label> = labels
            .filter(|_| preferences.label.enabled)
            .map(|labels| {
                labels
                    .items
                    .iter()
                    .filter(|item| preferences.quick_labels.contains(&item.name))
                    .collect()
            })
            .unwrap_or_default();
        let mut quick = self.quick.borrow_mut();
        if !quick
            .iter()
            .map(|(name, _)| name.as_str())
            .eq(wanted.iter().map(|item| item.name.as_str()))
        {
            for (_, item) in quick.drain(..) {
                self.menu.removeItem(&item);
                forget([item.tag()], target);
            }
            let index = self.menu.indexOfItem(&self.labels);
            for (offset, label) in wanted.iter().enumerate() {
                let item = menu_item(&label.name, Some(quick_label_request(pr, label)), target);
                self.menu.insertItem_atIndex(&item, index + offset as isize);
                quick.push((label.name.clone(), item));
            }
        }
        for ((_, item), label) in quick.iter().zip(&wanted) {
            item.setState(if label.selected {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            item.setEnabled(
                allowed && labels.is_some_and(|labels| !labels.pending.contains(&label.name)),
            );
            bind_item(item, quick_label_request(pr, label), target);
        }
    }
    pub fn update(
        &self,
        pr: &PullRequest,
        ignored: bool,
        state: &ActionState,
        target: &ActionTarget,
    ) {
        // Freeze the PR/head and enabled state the user actually saw in this menu.
        // The worker independently revalidates it before executing a mutation.
        if self.is_tracking() {
            return;
        }
        let progress = state.merges.get(&pr.snapshot.id);
        let busy = progress.is_some_and(|p| p.busy() || *p == MergeProgress::Complete);
        self.button.setHidden(ignored || busy);
        self.cancel.setHidden(ignored || !busy);
        self.cancel.setEnabled(matches!(
            progress,
            Some(
                MergeProgress::WaitingForChecks
                    | MergeProgress::Countdown(_)
                    | MergeProgress::Queued(_)
                    | MergeProgress::Checking
            )
        ));
        if let Some(progress) = progress {
            self.cancel.setTitle(&NSString::from_str(
                &if *progress == MergeProgress::Checking {
                    "Cancel (checking…)".into()
                } else {
                    progress.text()
                },
            ));
        }
        let preferences = state.preferences(&pr.snapshot.repo);
        let ready_pending = state.ready_pending(&pr.snapshot.id);
        self.ready.setHidden(!pr.snapshot.draft);
        self.ready
            .setEnabled(pr.snapshot.draft && pr.snapshot.open && !pr.stale && !ready_pending);
        self.merge.setTitle(&NSString::from_str(
            if pr.snapshot.check_state == Some(crate::model::CheckState::Running) {
                "Merge on green checks"
            } else {
                "Merge"
            },
        ));
        self.merge.setHidden(!preferences.merge.enabled);
        self.labels.setHidden(!preferences.label.enabled);
        self.separator.setHidden(
            !pr.snapshot.draft && !preferences.merge.enabled && !preferences.label.enabled,
        );
        // GitHub still has a draft until the optimistically shown Ready for review saves.
        self.merge
            .setEnabled(preferences.allows(Kind::Merge, pr) && !busy && !ready_pending);
        let labels_allowed = preferences.allows(Kind::Label, pr);
        self.labels.setEnabled(labels_allowed);
        self.update_quick_labels(pr, state, labels_allowed, target);
        // Reports compare against the PR as GitHub has it, so the optimistic
        // Ready for review display and in-flight merges cannot be reported.
        let checking = state.reports.get(&pr.snapshot.id) == Some(&ReportProgress::Checking);
        self.report.setTitle(&NSString::from_str(if checking {
            "Checking report…"
        } else {
            REPORT
        }));
        self.report
            .setEnabled(!checking && !ready_pending && !busy && !pr.snapshot.url.is_empty());
        bind_item(&self.report, report_request(pr), target);
        bind_item(
            &self.ready,
            AppEvent::PrAction(Request::ReadyForReview {
                pr: pr.snapshot.id.clone(),
                head: pr.snapshot.head.clone(),
                update: pr.update_id.clone(),
            }),
            target,
        );
        bind_item(
            &self.configure,
            AppEvent::PopoverAction(Action::ConfigureRepo(pr.snapshot.repo.clone())),
            target,
        );
        bind_item(
            &self.labels,
            AppEvent::PopoverAction(Action::Labels {
                pr: pr.snapshot.id.clone(),
                update: pr.update_id.clone(),
            }),
            target,
        );
        bind_item(
            &self.merge,
            AppEvent::PrAction(Request::Merge {
                on_green: pr.snapshot.check_state == Some(crate::model::CheckState::Running),
                pr: pr.snapshot.id.clone(),
                head: pr.snapshot.head.clone(),
                update: pr.update_id.clone(),
            }),
            target,
        );
    }
    pub fn is_tracking(&self) -> bool {
        self.tracker.ivars().tracking.get()
    }
    pub fn remove(&self, target: &ActionTarget) {
        forget(
            [
                self.configure.tag(),
                self.ready.tag(),
                self.merge.tag(),
                self.labels.tag(),
                self.report.tag(),
                self.ignore.tag(),
                self.cancel.tag(),
            ]
            .into_iter()
            .chain(self.quick.borrow().iter().map(|(_, item)| item.tag())),
            target,
        );
    }
}

struct ConfigRow {
    kind: Kind,
    enabled: Retained<NSButton>,
    condition: Retained<NSPopUpButton>,
    method: Option<Retained<NSPopUpButton>>,
}
pub(super) struct ConfigEditor {
    repo: String,
    view: Retained<FlippedView>,
    rows: Vec<ConfigRow>,
    tags: Vec<isize>,
}
fn choice(
    options: Vec<(&str, Setting)>,
    repo: &str,
    frame: NSRect,
    target: &ActionTarget,
    tags: &mut Vec<isize>,
) -> Retained<NSPopUpButton> {
    let mtm = MainThreadMarker::new().unwrap();
    let button = NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(mtm), frame, false);
    button.setFont(Some(&NSFont::systemFontOfSize(12.0)));
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    for (title, change) in options {
        let item = menu_item(
            title,
            Some(AppEvent::PrAction(Request::Configure {
                repo: repo.into(),
                change,
            })),
            target,
        );
        tags.push(item.tag());
        menu.addItem(&item);
    }
    button.setMenu(Some(&menu));
    button
}
impl ConfigEditor {
    fn new(repo: &str, target: &ActionTarget) -> Self {
        let mtm = MainThreadMarker::new().unwrap();
        let view = FlippedView::new(rect(0.0, 0.0, WIDTH, HEIGHT - 82.0), mtm);
        let mut rows = Vec::new();
        let mut tags = Vec::new();
        for (text, x, width) in [
            ("Action", 20.0, 90.0),
            ("Enabled", 126.0, 72.0),
            ("Condition", 220.0, 126.0),
            ("Merge method", 384.0, 176.0),
        ] {
            let heading = label(text, 12.0, true, mtm);
            heading.setFrame(rect(x, 12.0, width, 24.0));
            view.addSubview(&heading);
        }
        for (index, kind) in [Kind::Merge, Kind::Label].into_iter().enumerate() {
            let y = 48.0 + index as f64 * 48.0;
            let name = label(
                if kind == Kind::Merge {
                    "Merge"
                } else {
                    "Labels"
                },
                13.0,
                false,
                mtm,
            );
            name.setFrame(rect(20.0, y + 4.0, 90.0, 24.0));
            view.addSubview(&name);
            let enabled = target.button(
                "",
                AppEvent::PrAction(Request::Configure {
                    repo: repo.into(),
                    change: Setting::Enabled(kind, false),
                }),
                mtm,
            );
            enabled.setButtonType(NSButtonType::Switch);
            enabled.setFrame(rect(145.0, y, 26.0, 28.0));
            enabled.setAccessibilityLabel(Some(&NSString::from_str(&format!(
                "Enable {}",
                if kind == Kind::Merge {
                    "Merge"
                } else {
                    "Labels"
                }
            ))));
            tags.push(enabled.tag());
            view.addSubview(&enabled);
            let condition = choice(
                Condition::ALL
                    .into_iter()
                    .map(|condition| (condition.label(), Setting::Condition(kind, condition)))
                    .collect(),
                repo,
                rect(214.0, y, 142.0, 28.0),
                target,
                &mut tags,
            );
            view.addSubview(&condition);
            let method = (kind == Kind::Merge).then(|| {
                let button = choice(
                    MergeMethod::ALL
                        .into_iter()
                        .map(|method| (method.label(), Setting::MergeMethod(method)))
                        .collect(),
                    repo,
                    rect(378.0, y, 182.0, 28.0),
                    target,
                    &mut tags,
                );
                view.addSubview(&button);
                button
            });
            rows.push(ConfigRow {
                kind,
                enabled,
                condition,
                method,
            });
        }
        Self {
            repo: repo.into(),
            view,
            rows,
            tags,
        }
    }
    fn update(&self, state: &ActionState) {
        let preferences = state.preferences(&self.repo);
        for row in &self.rows {
            let rule = preferences.rule(row.kind);
            row.enabled.setState(if rule.enabled {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            row.condition.selectItemAtIndex(
                Condition::ALL
                    .iter()
                    .position(|condition| *condition == rule.condition)
                    .unwrap() as isize,
            );
            if let Some(method) = &row.method {
                method.selectItemAtIndex(
                    MergeMethod::ALL
                        .iter()
                        .position(|method| *method == preferences.merge_method)
                        .unwrap() as isize,
                );
            }
        }
    }
}

struct LabelRow {
    checkbox: Retained<NSButton>,
    dot: Retained<NSImageView>,
    unsaved: Retained<NSButton>,
    quick: Retained<NSButton>,
}
impl LabelRow {
    fn tags(&self) -> [isize; 3] {
        [self.checkbox.tag(), self.unsaved.tag(), self.quick.tag()]
    }
}
pub(super) struct LabelPicker {
    pub(super) id: String,
    /// The PR's currently displayed update, acknowledged when labels save.
    pub(super) update: String,
    repo: String,
    title: String,
    view: Retained<FlippedView>,
    message: Retained<NSTextField>,
    refresh: Retained<NSButton>,
    quick_heading: Retained<NSTextField>,
    rows: HashMap<String, LabelRow>,
}
impl LabelPicker {
    fn new(pr: &PullRequest, target: &ActionTarget) -> Self {
        let mtm = MainThreadMarker::new().unwrap();
        let view = FlippedView::new(rect(0.0, 0.0, WIDTH, HEIGHT - 82.0), mtm);
        let message = label("Choose labels. Back saves your changes.", 12.0, true, mtm);
        view.addSubview(&message);
        let refresh = target.button(
            "Refresh labels",
            AppEvent::PrAction(Request::LoadLabels(pr.snapshot.id.clone())),
            mtm,
        );
        refresh.setFrame(rect(WIDTH - 144.0, 8.0, 124.0, 28.0));
        view.addSubview(&refresh);
        let quick = label("Quick", 12.0, true, mtm);
        quick.setAlignment(NSTextAlignment::Center);
        quick.setHidden(true);
        view.addSubview(&quick);
        Self {
            id: pr.snapshot.id.clone(),
            update: pr.update_id.clone(),
            repo: pr.snapshot.repo.clone(),
            title: format!(
                "Labels #{} · {}",
                pr.snapshot.number,
                repo_heading(&pr.snapshot.repo)
            ),
            view,
            message,
            refresh,
            quick_heading: quick,
            rows: HashMap::new(),
        }
    }
    fn update(&mut self, prs: &[PullRequest], state: &ActionState, target: &ActionTarget) {
        let mtm = MainThreadMarker::new().unwrap();
        let labels = state.labels.get(&self.id).cloned().unwrap_or_default();
        let pr = prs.iter().find(|pr| pr.snapshot.id == self.id);
        if let Some(pr) = pr {
            self.update.clone_from(&pr.update_id);
        }
        let allowed =
            pr.is_some_and(|pr| state.preferences(&pr.snapshot.repo).allows(Kind::Label, pr));
        self.refresh
            .setEnabled(allowed && !labels.loading && labels.pending.is_empty());
        let text = if let Some(error) = labels.error.as_ref().or(labels.catalogue_error.as_ref()) {
            error.as_str()
        } else if !allowed {
            "Label action unavailable for this PR's current state."
        } else if !labels.pending.is_empty() {
            "Saving label changes…"
        } else if !labels.catalogue_ready {
            "Showing known labels. The repository catalogue refreshes in the background."
        } else if labels.items.is_empty() {
            "This repository has no labels."
        } else {
            "Choose labels. Back saves your changes."
        };
        set_text(&self.message, text);
        let height = self
            .message
            .sizeThatFits(NSSize::new(WIDTH - 180.0, 10000.0))
            .height;
        self.message
            .setFrame(rect(20.0, 12.0, WIDTH - 180.0, height));
        let names: HashSet<_> = labels
            .items
            .iter()
            .map(|label| label.name.as_str())
            .collect();
        self.rows.retain(|name, row| {
            if names.contains(name.as_str()) {
                true
            } else {
                row.checkbox.removeFromSuperview();
                row.dot.removeFromSuperview();
                row.unsaved.removeFromSuperview();
                row.quick.removeFromSuperview();
                forget(row.tags(), target);
                false
            }
        });
        let quick_labels = state.preferences(&self.repo).quick_labels;
        // Leave room below Refresh labels for the Quick column heading.
        let mut y = (height + 28.0).max(60.0);
        self.quick_heading
            .setFrame(rect(WIDTH - 83.0, y - 20.0, 60.0, 18.0));
        self.quick_heading.setHidden(labels.items.is_empty());
        for item in &labels.items {
            let row = self.rows.entry(item.name.clone()).or_insert_with(|| {
                let checkbox = target.button(
                    &item.name,
                    AppEvent::PrAction(Request::Label {
                        pr: self.id.clone(),
                        name: item.name.clone(),
                        selected: !item.selected,
                    }),
                    mtm,
                );
                checkbox.setButtonType(NSButtonType::Switch);
                checkbox.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
                checkbox.setToolTip(Some(&NSString::from_str(&item.name)));
                let dot =
                    NSImageView::initWithFrame(NSImageView::alloc(mtm), rect(0.0, 0.0, 12.0, 12.0));
                let unsaved = target.button(
                    "Unsaved",
                    AppEvent::PrAction(Request::SaveLabels {
                        pr: self.id.clone(),
                        name: Some(item.name.clone()),
                        update: self.update.clone(),
                    }),
                    mtm,
                );
                unsaved.setBordered(false);
                unsaved.setFont(Some(&NSFont::systemFontOfSize(11.0)));
                unsaved.setContentTintColor(Some(&NSColor::secondaryLabelColor()));
                if let Some(image) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                    &NSString::from_str("circle.fill"),
                    None,
                ) {
                    image.setTemplate(true);
                    dot.setImage(Some(&image));
                }
                // Repository-wide: a quick label appears directly in the Actions
                // menu of every PR in this repository.
                let quick = target.button(
                    "",
                    AppEvent::PrAction(Request::Configure {
                        repo: self.repo.clone(),
                        change: Setting::QuickLabel(item.name.clone(), false),
                    }),
                    mtm,
                );
                quick.setButtonType(NSButtonType::Switch);
                quick.setAccessibilityLabel(Some(&NSString::from_str(&format!(
                    "Quick label {}",
                    item.name
                ))));
                quick.setToolTip(Some(&NSString::from_str(
                    "Show this label directly in the PR Actions menu",
                )));
                self.view.addSubview(&checkbox);
                self.view.addSubview(&dot);
                self.view.addSubview(&unsaved);
                self.view.addSubview(&quick);
                LabelRow {
                    checkbox,
                    dot,
                    unsaved,
                    quick,
                }
            });
            row.checkbox.setFrame(rect(44.0, y, WIDTH - 228.0, 28.0));
            row.dot.setFrame(rect(23.0, y + 8.0, 12.0, 12.0));
            row.dot.setContentTintColor(Some(&label_color(&item.color)));
            row.unsaved.setFrame(rect(WIDTH - 176.0, y, 80.0, 28.0));
            row.quick.setFrame(rect(WIDTH - 61.0, y, 26.0, 28.0));
            row.quick.setState(if quick_labels.contains(&item.name) {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            target.bind(
                &row.unsaved,
                AppEvent::PrAction(Request::SaveLabels {
                    pr: self.id.clone(),
                    name: Some(item.name.clone()),
                    update: self.update.clone(),
                }),
            );
            row.unsaved.setHidden(!labels.unsaved.contains(&item.name));
            row.unsaved
                .setEnabled(allowed && !labels.pending.contains(&item.name));
            row.checkbox.setState(if item.selected {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            row.checkbox
                .setEnabled(allowed && !labels.pending.contains(&item.name));
            y += 32.0;
        }
        self.view
            .setFrameSize(NSSize::new(WIDTH, (y + 20.0).max(HEIGHT - 82.0)));
    }
}
pub(super) fn label_color(hex: &str) -> Retained<NSColor> {
    let rgb = if hex.len() == 6 {
        u32::from_str_radix(hex, 16).ok()
    } else {
        None
    }
    .unwrap_or(0x808080);
    NSColor::colorWithSRGBRed_green_blue_alpha(
        ((rgb >> 16) & 255) as f64 / 255.0,
        ((rgb >> 8) & 255) as f64 / 255.0,
        (rgb & 255) as f64 / 255.0,
        1.0,
    )
}

pub(super) enum DetailPanel {
    Configuration(ConfigEditor),
    Labels(LabelPicker),
    Settings(super::settings::SettingsEditor),
}
impl DetailPanel {
    pub fn settings(target: &ActionTarget) -> Self {
        Self::Settings(super::settings::SettingsEditor::new(target))
    }
    pub fn configuration(repo: &str, target: &ActionTarget) -> Self {
        Self::Configuration(ConfigEditor::new(repo, target))
    }
    pub fn labels(pr: &PullRequest, target: &ActionTarget) -> Self {
        Self::Labels(LabelPicker::new(pr, target))
    }
    pub fn view(&self) -> &NSView {
        match self {
            Self::Configuration(editor) => &editor.view,
            Self::Labels(picker) => &picker.view,
            Self::Settings(editor) => &editor.view,
        }
    }
    pub fn title(&self) -> String {
        match self {
            Self::Configuration(editor) => format!("Configure {}", repo_heading(&editor.repo)),
            Self::Labels(picker) => picker.title.clone(),
            Self::Settings(_) => "Settings".into(),
        }
    }
    pub fn labels_saving(&self, state: &ActionState) -> bool {
        match self {
            Self::Labels(picker) => state
                .labels
                .get(&picker.id)
                .is_some_and(|labels| !labels.pending.is_empty()),
            Self::Configuration(_) | Self::Settings(_) => false,
        }
    }
    pub fn label_error(&self, state: &ActionState) -> bool {
        match self {
            Self::Labels(picker) => state
                .labels
                .get(&picker.id)
                .is_some_and(|labels| labels.error.is_some()),
            Self::Configuration(_) | Self::Settings(_) => false,
        }
    }
    pub fn subtitle(&self) -> &'static str {
        match self {
            Self::Configuration(_) => "Changes save automatically for this repository.",
            Self::Labels(_) => "Changes save when you go back.",
            Self::Settings(_) => "Customize Gopher’s keyboard shortcuts and updates.",
        }
    }
    pub fn update(&mut self, prs: &[PullRequest], state: &ActionState, target: &ActionTarget) {
        match self {
            Self::Configuration(editor) => editor.update(state),
            Self::Labels(picker) => picker.update(prs, state, target),
            Self::Settings(_) => {}
        }
    }
    pub fn remove(self, target: &ActionTarget) {
        match self {
            Self::Configuration(editor) => forget(editor.tags, target),
            Self::Settings(editor) => editor.remove(target),
            Self::Labels(picker) => {
                forget([picker.refresh.tag()], target);
                forget(picker.rows.values().flat_map(LabelRow::tags), target);
            }
        }
    }
}
