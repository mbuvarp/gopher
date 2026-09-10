use super::*;
use crate::actions::{Condition, Kind, MergeMethod, MergeProgress};
use objc2::runtime::ProtocolObject;
use std::cell::Cell;

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

pub(super) struct PrActions {
    pub button: Retained<NSPopUpButton>,
    pub cancel: Retained<NSButton>,
    configure: Retained<NSMenuItem>,
    merge: Retained<NSMenuItem>,
    labels: Retained<NSMenuItem>,
    separator: Retained<NSMenuItem>,
    ignore: Retained<NSMenuItem>,
    tracker: Retained<MenuTracker>,
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
                pr: pr.snapshot.id.clone(),
                head: pr.snapshot.head.clone(),
                update: pr.update_id.clone(),
            })),
            target,
        );
        let labels = menu_item(
            "Label",
            Some(AppEvent::PopoverAction(Action::Labels(
                pr.snapshot.id.clone(),
            ))),
            target,
        );
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
        menu.addItem(&merge);
        menu.addItem(&labels);
        menu.addItem(&separator);
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
            merge,
            labels,
            separator,
            ignore,
            tracker,
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
            Some(MergeProgress::Countdown(_) | MergeProgress::Checking)
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
        self.merge.setHidden(!preferences.merge.enabled);
        self.labels.setHidden(!preferences.label.enabled);
        self.separator
            .setHidden(!preferences.merge.enabled && !preferences.label.enabled);
        self.merge
            .setEnabled(preferences.allows(Kind::Merge, pr) && !busy);
        self.labels.setEnabled(preferences.allows(Kind::Label, pr));
        bind_item(
            &self.configure,
            AppEvent::PopoverAction(Action::ConfigureRepo(pr.snapshot.repo.clone())),
            target,
        );
        bind_item(
            &self.merge,
            AppEvent::PrAction(Request::Merge {
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
                self.merge.tag(),
                self.labels.tag(),
                self.ignore.tag(),
                self.cancel.tag(),
            ],
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
                    "Label"
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
                    "Label"
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
}
pub(super) struct LabelPicker {
    id: String,
    title: String,
    view: Retained<FlippedView>,
    message: Retained<NSTextField>,
    refresh: Retained<NSButton>,
    rows: HashMap<String, LabelRow>,
}
impl LabelPicker {
    fn new(pr: &PullRequest, target: &ActionTarget) -> Self {
        let mtm = MainThreadMarker::new().unwrap();
        let view = FlippedView::new(rect(0.0, 0.0, WIDTH, HEIGHT - 82.0), mtm);
        let message = label("Loading labels…", 12.0, true, mtm);
        view.addSubview(&message);
        let refresh = target.button(
            "Refresh labels",
            AppEvent::PrAction(Request::LoadLabels(pr.snapshot.id.clone())),
            mtm,
        );
        refresh.setFrame(rect(WIDTH - 144.0, 8.0, 124.0, 28.0));
        view.addSubview(&refresh);
        Self {
            id: pr.snapshot.id.clone(),
            title: format!(
                "Labels #{} · {}",
                pr.snapshot.number,
                repo_heading(&pr.snapshot.repo)
            ),
            view,
            message,
            refresh,
            rows: HashMap::new(),
        }
    }
    fn update(&mut self, prs: &[PullRequest], state: &ActionState, target: &ActionTarget) {
        let mtm = MainThreadMarker::new().unwrap();
        let labels = state.labels.get(&self.id).cloned().unwrap_or_default();
        let allowed = prs
            .iter()
            .find(|pr| pr.snapshot.id == self.id)
            .is_some_and(|pr| state.preferences(&pr.snapshot.repo).allows(Kind::Label, pr));
        self.refresh
            .setEnabled(allowed && !labels.loading && labels.pending.is_empty());
        let text = if let Some(error) = &labels.error {
            error.as_str()
        } else if !allowed {
            "Label action unavailable for this PR's current state."
        } else if labels.loading {
            "Loading labels…"
        } else if !labels.pending.is_empty() {
            "Saving label changes…"
        } else if labels.items.is_empty() {
            "This repository has no labels."
        } else {
            "Toggle labels to add or remove them."
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
                forget([row.checkbox.tag()], target);
                false
            }
        });
        let mut y = (height + 28.0).max(52.0);
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
                if let Some(image) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                    &NSString::from_str("circle.fill"),
                    None,
                ) {
                    image.setTemplate(true);
                    dot.setImage(Some(&image));
                }
                self.view.addSubview(&checkbox);
                self.view.addSubview(&dot);
                LabelRow { checkbox, dot }
            });
            row.checkbox.setFrame(rect(44.0, y, WIDTH - 64.0, 28.0));
            row.dot.setFrame(rect(23.0, y + 8.0, 12.0, 12.0));
            row.dot.setContentTintColor(Some(&label_color(&item.color)));
            row.checkbox.setState(if item.selected {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            row.checkbox
                .setEnabled(allowed && !labels.loading && !labels.pending.contains(&item.name));
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
}
impl DetailPanel {
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
        }
    }
    pub fn title(&self) -> String {
        match self {
            Self::Configuration(editor) => format!("Configure {}", repo_heading(&editor.repo)),
            Self::Labels(picker) => picker.title.clone(),
        }
    }
    pub fn subtitle(&self) -> &'static str {
        match self {
            Self::Configuration(_) => "Changes save automatically for this repository.",
            Self::Labels(_) => "Changes apply to this pull request.",
        }
    }
    pub fn update(&mut self, prs: &[PullRequest], state: &ActionState, target: &ActionTarget) {
        match self {
            Self::Configuration(editor) => editor.update(state),
            Self::Labels(picker) => picker.update(prs, state, target),
        }
    }
    pub fn remove(self, target: &ActionTarget) {
        match self {
            Self::Configuration(editor) => forget(editor.tags, target),
            Self::Labels(picker) => {
                forget([picker.refresh.tag()], target);
                forget(picker.rows.values().map(|row| row.checkbox.tag()), target);
            }
        }
    }
}
