//! Native, persistent review inbox. Controls retain their identity across polls;
//! actions capture the update displayed at activation, before entering the actor.
use super::{Action, AppEvent, repo_heading, repo_parts};
use crate::model::{CheckState, PullRequest, State};
use crate::{
    actions::{ActionState, Request, Setting},
    worker::{ActionCommand, Command},
};
use tokio::sync::mpsc::UnboundedSender;
mod action_views;
mod attention;
mod label_pills;
mod settings;
use crate::hotkeys::{HotkeyAction, Navigation};
use action_views::{DetailPanel, PrActions};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained, sel,
};
use objc2_app_kit::*;
use objc2_foundation::{
    NSMutableAttributedString, NSObject, NSObjectProtocol, NSPoint, NSRange, NSRect, NSRectEdge,
    NSSize, NSString,
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
};
use tao::event_loop::EventLoopProxy;
use tray_icon::TrayIcon;

const WIDTH: f64 = 580.0;
const HEIGHT: f64 = 620.0;
const CONTENT_WIDTH: f64 = WIDTH - 32.0;
const HIGHLIGHT_PADDING: f64 = 8.0;

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    struct FlippedView;
    unsafe impl NSObjectProtocol for FlippedView {}
    impl FlippedView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool { true }
    }
);
impl FlippedView {
    fn new(frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send![super(Self::alloc(mtm).set_ivars(())), initWithFrame: frame] }
    }
}

struct TargetState {
    proxy: EventLoopProxy<AppEvent>,
    actions: RefCell<HashMap<isize, AppEvent>>,
    next_tag: std::cell::Cell<isize>,
    pending_label_requests: std::cell::Cell<usize>,
    sender: UnboundedSender<Command>,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = TargetState]
    struct ActionTarget;
    unsafe impl NSObjectProtocol for ActionTarget {}
    impl ActionTarget {
        #[unsafe(method(activate:))]
        fn activate(&self, sender: &objc2::runtime::AnyObject) {
            // All callers are controls or menu items with the documented tag property.
            let tag: isize = unsafe { msg_send![sender, tag] };
            if let Some(event) = self.ivars().actions.borrow().get(&tag).cloned() {
                if let AppEvent::PrAction(mut request) = event {
                    // Checkbox state is the user's latest click, even before the
                    // worker has persisted an earlier click on the same control.
                    match &mut request {
                        Request::Configure { change: Setting::Enabled(_, value), .. } | Request::Label { selected: value, .. } => {
                            let state: isize = unsafe { msg_send![sender, state] };
                            *value = state == NSControlStateValueOn;
                        }
                        _ => {}
                    }
                    // Hold navigation immediately, before worker state reaches the UI.
                    // Its acknowledgement follows the corresponding ActionsChanged event.
                    let label_request = matches!(&request, Request::Label { .. });
                    if label_request {
                        let pending = &self.ivars().pending_label_requests;
                        pending.set(pending.get() + 1);
                        let _ = self.ivars().proxy.send_event(AppEvent::RefreshPopover);
                    }
                    // Direct dispatch also works while AppKit tracks a menu.
                    if self.ivars().sender.send(Command::PrAction(ActionCommand::Request(request))).is_err() && label_request {
                        let pending = &self.ivars().pending_label_requests;
                        pending.set(pending.get().saturating_sub(1));
                    }
                } else {
                    if let AppEvent::PopoverAction(Action::Ack { pr, .. }) = &event { let _ = self.ivars().proxy.send_event(AppEvent::HighlightPr(pr.clone())); }
                    let _ = self.ivars().proxy.send_event(event);
                }
            }
        }
    }
);
impl ActionTarget {
    fn new(
        proxy: EventLoopProxy<AppEvent>,
        sender: UnboundedSender<Command>,
        mtm: MainThreadMarker,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetState {
            proxy,
            actions: RefCell::new(HashMap::new()),
            next_tag: std::cell::Cell::new(1),
            pending_label_requests: std::cell::Cell::new(0),
            sender,
        });
        unsafe { msg_send![super(this), init] }
    }
    fn register(&self, event: AppEvent) -> isize {
        let tag = self.ivars().next_tag.get();
        self.ivars().next_tag.set(tag + 1);
        self.ivars().actions.borrow_mut().insert(tag, event);
        tag
    }
    fn bind(&self, button: &NSButton, event: AppEvent) {
        self.ivars()
            .actions
            .borrow_mut()
            .insert(button.tag(), event);
    }
    fn button(&self, title: &str, event: AppEvent, mtm: MainThreadMarker) -> Retained<NSButton> {
        // The popover retains this target longer than all its controls.
        let button = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str(title),
                Some(self),
                Some(sel!(activate:)),
                mtm,
            )
        };
        button.setTag(self.register(event));
        button.setFont(Some(&NSFont::systemFontOfSize(12.0)));
        button
    }
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}
fn label(text: &str, size: f64, secondary: bool, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let field = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
    field.setFont(Some(&NSFont::systemFontOfSize(size)));
    if secondary {
        field.setTextColor(Some(&NSColor::secondaryLabelColor()));
    }
    field
}
fn set_text(field: &NSTextField, text: &str) {
    if field.stringValue().to_string() != text {
        field.setStringValue(&NSString::from_str(text));
    }
}
fn set_status(field: &NSTextField, text: &str, checks: Option<CheckState>) {
    let suffix = checks
        .map(|state| format!(" · {}", state.label()))
        .unwrap_or_default();
    let value = NSMutableAttributedString::initWithString(
        NSMutableAttributedString::alloc(),
        &NSString::from_str(&format!("{text}{suffix}")),
    );
    // AppKit ranges use UTF-16 offsets. Reset the base attributes on every update
    // so a previous check color cannot spill into review text or cached states.
    unsafe {
        let all = NSRange::new(0, value.length());
        value.addAttribute_value_range(NSFontAttributeName, &NSFont::systemFontOfSize(11.0), all);
        value.addAttribute_value_range(
            NSForegroundColorAttributeName,
            &NSColor::secondaryLabelColor(),
            all,
        );
        if let Some(state) = checks {
            let color = match state {
                CheckState::Running => NSColor::systemYellowColor(),
                CheckState::Failed => NSColor::systemRedColor(),
                CheckState::Green => NSColor::systemGreenColor(),
            };
            let length = state.label().encode_utf16().count();
            value.addAttribute_value_range(
                NSForegroundColorAttributeName,
                &color,
                NSRange::new(value.length() - length, length),
            );
        }
    }
    field.setAttributedStringValue(&value);
}
fn display_title(pr: &PullRequest) -> String {
    if pr.snapshot.number == 0 {
        return format!("Unavailable PR · {}", pr.snapshot.id);
    }
    let title: String = pr
        .snapshot
        .title
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    format!("#{}  {}", pr.snapshot.number, title)
}
fn details(pr: &PullRequest) -> String {
    let mut lines = Vec::new();
    for agent in &pr.agents {
        lines.push(format!(
            "{} · {:?}\n{}",
            agent.agent.label(),
            agent.verdict,
            agent.reason
        ));
    }
    if lines.is_empty() {
        lines.push("No agent review activity detected.".into());
    }
    if let Some(error) = &pr.error {
        lines.push(error.clone());
    }
    if pr.fetched_at > 0
        && let Some(time) = chrono::DateTime::from_timestamp(pr.fetched_at, 0)
    {
        lines.push(format!(
            "Last checked {}",
            time.with_timezone(&chrono::Local).format("%H:%M:%S")
        ));
    }
    lines.join("\n\n")
}

struct Row {
    normal_blue: std::cell::Cell<bool>,
    background: Retained<NSBox>,
    view: Retained<FlippedView>,
    icon: Retained<NSImageView>,
    title: Retained<NSButton>,
    status: Retained<NSTextField>,
    labels: label_pills::LabelPills,
    open: Retained<NSButton>,
    disclosure: Retained<NSButton>,
    ignore: Retained<NSButton>,
    actions: PrActions,
    action_message: Retained<NSTextField>,
    details: Retained<NSTextField>,
}
impl Row {
    fn new(pr: &PullRequest, target: &ActionTarget, mtm: MainThreadMarker) -> Self {
        let id = &pr.snapshot.id;
        let placeholder = AppEvent::ToggleDetails(id.clone());
        let view = FlippedView::new(rect(16.0, 0.0, CONTENT_WIDTH, 100.0), mtm);
        let background = NSBox::new(mtm);
        background.setBoxType(NSBoxType::Custom);
        background.setTitlePosition(NSTitlePosition::NoTitle);
        background.setBorderWidth(0.0);
        background.setCornerRadius(8.0);
        background.setTransparent(false);
        background.setHidden(true);
        view.addSubview(&background);
        let icon = NSImageView::initWithFrame(NSImageView::alloc(mtm), rect(0.0, 4.0, 36.0, 48.0));
        icon.setImageScaling(NSImageScaling::ScaleProportionallyDown);
        let title = target.button("", placeholder.clone(), mtm);
        title.setBordered(false);
        title.setAlignment(NSTextAlignment::Left);
        title.setFont(Some(&NSFont::systemFontOfSize(13.0)));
        title.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        title.setFrame(rect(46.0, 4.0, CONTENT_WIDTH - 46.0, 28.0));
        let status = label("", 11.0, true, mtm);
        status.setMaximumNumberOfLines(1);
        status.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        status.setFrame(rect(49.0, 34.0, CONTENT_WIDTH - 52.0, 18.0));
        let open = target.button("Open PR", placeholder.clone(), mtm);
        open.setFrame(rect(0.0, 57.0, 84.0, 26.0));
        let disclosure = target.button("Details", placeholder.clone(), mtm);
        disclosure.setFrame(rect(94.0, 57.0, 92.0, 26.0));
        let ignore = target.button("Ignore", placeholder, mtm);
        ignore.setFrame(rect(196.0, 57.0, 72.0, 26.0));
        let details = label("", 12.0, true, mtm);
        let actions = PrActions::new(pr, target, mtm);
        let action_message = label("", 11.0, false, mtm);
        action_message.setTextColor(Some(&NSColor::systemRedColor()));
        details.setSelectable(true);
        for child in [
            &*icon as &NSView,
            &*title,
            &*status,
            &*open,
            &*disclosure,
            &*ignore,
            &*details,
            &*actions.button,
            &*actions.cancel,
            &*action_message,
        ] {
            view.addSubview(child);
        }
        Self {
            normal_blue: std::cell::Cell::new(false),
            background,
            view,
            icon,
            title,
            status,
            labels: Default::default(),
            open,
            disclosure,
            ignore,
            details,
            actions,
            action_message,
        }
    }
    fn set_blue(&self, blue: bool) {
        let tint = blue.then(NSColor::systemBlueColor);
        self.title.setContentTintColor(tint.as_deref());
        self.icon.setContentTintColor(tint.as_deref());
    }
    fn update(
        &self,
        pr: &PullRequest,
        expanded: bool,
        ignored: bool,
        target: &ActionTarget,
        action_state: &ActionState,
    ) -> f64 {
        let state = if pr.stale && !ignored {
            State::Unknown
        } else {
            pr.state
        };
        self.title.setTitle(&NSString::from_str(&display_title(pr)));
        let needs_attention = !ignored && pr.needs_attention();
        self.normal_blue.set(needs_attention);
        self.set_blue(needs_attention);
        let font = if needs_attention {
            NSFont::boldSystemFontOfSize(13.0)
        } else {
            NSFont::systemFontOfSize(13.0)
        };
        self.title.setFont(Some(&font));
        self.title.setToolTip(Some(&NSString::from_str(&if ignored {
            pr.snapshot.title.clone()
        } else {
            format!("{}\nClick to acknowledge this update", pr.snapshot.title)
        })));
        let symbol = match state {
            State::Unknown => "questionmark.circle",
            State::Reviewing => "arrow.triangle.2.circlepath",
            State::Comments => "text.bubble",
            State::Approved => "checkmark.circle",
        };
        if let Some(image) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str(symbol),
            Some(&NSString::from_str(state.label())),
        ) {
            // Symbol glyph size is controlled by its configuration, not NSImage.size.
            let configuration =
                NSImageSymbolConfiguration::configurationWithPointSize_weight(36.0, unsafe {
                    NSFontWeightRegular
                });
            let image = image
                .imageWithSymbolConfiguration(&configuration)
                .unwrap_or(image);
            image.setTemplate(true);
            self.icon.setImage(Some(&image));
        }
        let threads = pr.snapshot.threads.iter().filter(|t| !t.resolved).count();
        set_status(
            &self.status,
            &if ignored {
                "Ignored · not monitored".to_owned()
            } else {
                format!(
                    "{}{}{} · {} unresolved threads",
                    pr.status_label(chrono::Utc::now().timestamp()),
                    if pr.stale { " · cached" } else { "" },
                    if pr.snapshot.draft { " · draft" } else { "" },
                    threads
                )
            },
            if ignored || pr.stale {
                None
            } else {
                pr.snapshot.check_state
            },
        );
        let label_height = self
            .labels
            .update(&self.view, &self.status, &pr.snapshot.labels);
        for button in [
            &*self.open as &NSView,
            &*self.disclosure,
            &*self.ignore,
            &*self.actions.button,
            &*self.actions.cancel,
        ] {
            let frame = button.frame();
            button.setFrameOrigin(NSPoint::new(frame.origin.x, 57.0 + label_height));
        }
        self.title.setEnabled(!pr.stale && !ignored);
        self.open.setEnabled(!pr.snapshot.url.is_empty());
        self.ignore.setTitle(&NSString::from_str(if ignored {
            "Restore"
        } else {
            "Ignore"
        }));
        self.ignore.setHidden(!ignored);
        self.actions.update(pr, ignored, action_state, target);
        target.bind(
            &self.title,
            AppEvent::PopoverAction(Action::Ack {
                pr: pr.snapshot.id.clone(),
                update: pr.update_id.clone(),
                checked: true,
            }),
        );
        target.bind(
            &self.open,
            AppEvent::PopoverAction(Action::Open {
                pr: pr.snapshot.id.clone(),
                update: pr.update_id.clone(),
                url: pr.snapshot.url.clone(),
            }),
        );
        target.bind(
            &self.ignore,
            AppEvent::PopoverAction(if ignored {
                Action::Restore(pr.snapshot.id.clone())
            } else {
                Action::Ignore(pr.snapshot.id.clone())
            }),
        );
        self.disclosure.setTitle(&NSString::from_str(if expanded {
            "Hide details"
        } else {
            "Details"
        }));
        self.details.setHidden(!expanded);
        let action_error = if ignored {
            None
        } else {
            match action_state.merges.get(&pr.snapshot.id) {
                Some(crate::actions::MergeProgress::Failed(error)) => Some(error.as_str()),
                _ => None,
            }
        };
        self.action_message.setHidden(action_error.is_none());
        let mut extra = label_height;
        if let Some(error) = action_error {
            set_text(&self.action_message, error);
            let height = self
                .action_message
                .sizeThatFits(NSSize::new(CONTENT_WIDTH - 12.0, 10000.0))
                .height;
            self.action_message.setFrame(rect(
                4.0,
                88.0 + label_height,
                CONTENT_WIDTH - 12.0,
                height,
            ));
            extra += height + 8.0;
        }
        if expanded {
            set_text(&self.details, &details(pr));
            let height = self
                .details
                .sizeThatFits(NSSize::new(CONTENT_WIDTH - 12.0, 10000.0))
                .height;
            self.details
                .setFrame(rect(4.0, 94.0 + extra, CONTENT_WIDTH - 12.0, height));
            106.0 + height + extra
        } else {
            98.0 + extra
        }
    }
    fn remove(&self, target: &ActionTarget) {
        self.view.removeFromSuperview();
        self.actions.remove(target);
        for button in [&self.title, &self.open, &self.disclosure, &self.ignore] {
            target.ivars().actions.borrow_mut().remove(&button.tag());
        }
    }
}

struct RepositoryHeading {
    label: Retained<NSTextField>,
    divider: Retained<NSBox>,
}

pub(super) struct ReviewPopover {
    // Declare target last: native controls have non-retaining targets.
    popover: Retained<NSPopover>,
    document: Retained<FlippedView>,
    scroll: Retained<NSScrollView>,
    summary: Retained<NSTextField>,
    title: Retained<NSTextField>,
    refresh: Retained<NSButton>,
    actions: Retained<NSPopUpButton>,
    back: Retained<NSButton>,
    showing_ignored: bool,
    scroll_positions: [NSPoint; 2],
    restore_scroll: bool,
    pending_scroll: Option<String>,
    flash: Option<attention::Flash>,
    navigation: [Navigation; 2],
    pub(super) keyboard_state: super::keyboard::State,
    banner: Retained<NSTextField>,
    empty: Retained<NSTextField>,
    login: Retained<NSMenuItem>,
    rows: HashMap<String, Row>,
    headings: HashMap<String, RepositoryHeading>,
    expanded: [HashSet<String>; 2],
    detail: Option<DetailPanel>,
    pub(super) action_state: ActionState,
    target: Retained<ActionTarget>,
}
impl ReviewPopover {
    pub(super) fn new(
        proxy: EventLoopProxy<AppEvent>,
        bundled: bool,
        sender: UnboundedSender<Command>,
    ) -> Self {
        let mtm = MainThreadMarker::new().expect("Popover must be created on the main thread");
        let target = ActionTarget::new(proxy, sender, mtm);
        let content = FlippedView::new(rect(0.0, 0.0, WIDTH, HEIGHT), mtm);
        let title = label("Gopher", 21.0, false, mtm);
        title.setFont(Some(&NSFont::boldSystemFontOfSize(21.0)));
        title.setFrame(rect(20.0, 16.0, 200.0, 28.0));
        content.addSubview(&title);
        let summary = label("Waiting for GitHub…", 12.0, true, mtm);
        summary.setFrame(rect(20.0, 48.0, 330.0, 20.0));
        content.addSubview(&summary);
        let refresh = target.button("Refresh", AppEvent::PopoverAction(Action::Refresh), mtm);
        refresh.setFrame(rect(WIDTH - 218.0, 24.0, 110.0, 28.0));
        content.addSubview(&refresh);
        let back = target.button("Back", AppEvent::PopoverAction(Action::BackToActive), mtm);
        back.setFrame(rect(WIDTH - 104.0, 24.0, 90.0, 28.0));
        back.setHidden(true);
        content.addSubview(&back);
        let actions = NSPopUpButton::initWithFrame_pullsDown(
            NSPopUpButton::alloc(mtm),
            rect(WIDTH - 104.0, 24.0, 90.0, 28.0),
            true,
        );
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        let heading = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Actions"),
                None,
                &NSString::from_str(""),
            )
        };
        menu.addItem(&heading);
        let mut login = None;
        for entry in [
            Some(("Show ignored", Action::ShowIgnored)),
            None,
            Some(("Settings", Action::Config)),
            Some(("Notification settings", Action::NotificationSettings)),
            Some(("Open logs", Action::Logs)),
            None,
            Some(("Launch at login", Action::Login)),
            Some(("Quit Gopher", Action::Quit)),
        ] {
            let Some((title, action)) = entry else {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                continue;
            };
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(title),
                    Some(sel!(activate:)),
                    &NSString::from_str(""),
                )
            };
            item.setTag(target.register(AppEvent::PopoverAction(action)));
            unsafe {
                item.setTarget(Some(&target));
            }
            if title == "Launch at login" {
                item.setEnabled(bundled);
                login = Some(item.clone());
            }
            menu.addItem(&item);
        }
        actions.setMenu(Some(&menu));
        content.addSubview(&actions);
        let scroll = NSScrollView::initWithFrame(
            NSScrollView::alloc(mtm),
            rect(0.0, 82.0, WIDTH, HEIGHT - 82.0),
        );
        scroll.setHasVerticalScroller(true);
        scroll.setDrawsBackground(false);
        let document = FlippedView::new(rect(0.0, 0.0, WIDTH, HEIGHT - 82.0), mtm);
        scroll.setDocumentView(Some(&document));
        content.addSubview(&scroll);
        let banner = label("", 12.0, false, mtm);
        banner.setTextColor(Some(&NSColor::systemOrangeColor()));
        banner.setSelectable(true);
        document.addSubview(&banner);
        let empty = label(
            "No open pull requests. Gopher will keep checking for you.",
            13.0,
            true,
            mtm,
        );
        document.addSubview(&empty);
        let controller = NSViewController::new(mtm);
        controller.setView(&content);
        let popover = NSPopover::new(mtm);
        popover.setContentViewController(Some(&controller));
        popover.setContentSize(NSSize::new(WIDTH, HEIGHT));
        popover.setBehavior(NSPopoverBehavior::Transient);
        Self {
            popover,
            document,
            scroll,
            summary,
            title,
            refresh,
            actions,
            back,
            showing_ignored: false,
            scroll_positions: [NSPoint::new(0.0, 0.0); 2],
            restore_scroll: false,
            pending_scroll: None,
            flash: None,
            navigation: Default::default(),
            keyboard_state: Default::default(),
            banner,
            empty,
            login: login.unwrap(),
            rows: HashMap::new(),
            headings: HashMap::new(),
            expanded: Default::default(),
            target,
            detail: None,
            action_state: ActionState::default(),
        }
    }
    pub(super) fn toggle(&mut self, tray: &TrayIcon) {
        if self.popover.isShown() {
            self.popover.close();
            return;
        }
        self.show(tray);
    }
    /// Notification navigation must never toggle an already open popover closed.
    pub(super) fn show(&mut self, tray: &TrayIcon) {
        if !self.popover.isShown() {
            for navigation in &mut self.navigation {
                navigation.selected = None;
            }
            for row in self.rows.values() {
                row.background.setHidden(true);
            }
        }
        // Support the macOS 13 deployment target as well as newer macOS releases.
        #[allow(deprecated)]
        NSApplication::sharedApplication(MainThreadMarker::new().unwrap())
            .activateIgnoringOtherApps(true);
        if let Some(item) = tray.ns_status_item()
            && let Some(button) = item.button(MainThreadMarker::new().unwrap())
        {
            if !self.popover.isShown() {
                self.popover.showRelativeToRect_ofView_preferredEdge(
                    button.bounds(),
                    &button,
                    NSRectEdge::MinY,
                );
            }
            if let Some(window) = self.scroll.window() {
                window.makeKeyWindow();
            }
            tracing::debug!(event = "popover_opened");
        }
    }
    pub(super) fn prepare_notification_reveal(&mut self) -> bool {
        // Defer navigation while controls are in use. A label-save failure must
        // remain visible until the user leaves that picker or retries successfully.
        if self.rows.values().any(|row| row.actions.is_tracking())
            || self.labels_saving()
            || self.keyboard_state.pending
            || self.keyboard_state.recording.is_some()
            || self
                .detail
                .as_ref()
                .is_some_and(|panel| panel.label_error(&self.action_state))
        {
            return false;
        }
        if self.detail.is_some() {
            self.back();
        }
        self.show_ignored(false);
        true
    }
    pub(super) fn scroll_to_pr(&mut self, pr: Option<String>) {
        self.stop_flash();
        self.flash = pr.clone().map(attention::Flash::new);
        self.navigation[usize::from(self.showing_ignored)].selected = pr.clone();
        self.pending_scroll = pr;
    }
    fn stop_flash(&mut self) {
        if let Some(flash) = self.flash.take()
            && let Some(row) = self.rows.get(&flash.pr)
        {
            row.set_blue(row.normal_blue.get());
        }
    }
    pub(super) fn animate(&mut self) -> Option<std::time::Instant> {
        let flash = self.flash.as_ref()?;
        if self.popover.isShown()
            && self.detail.is_none()
            && !self.showing_ignored
            && self.navigation[0].selected.as_ref() == Some(&flash.pr)
            && let Some(row) = self.rows.get(&flash.pr)
            && let Some((blue, next)) = flash.phase(std::time::Instant::now())
        {
            row.set_blue(blue);
            return Some(next);
        }
        self.stop_flash();
        None
    }
    pub(super) fn highlight(&mut self, id: String) {
        if self.rows.contains_key(&id) {
            self.navigation[usize::from(self.showing_ignored)].selected = Some(id);
        }
    }
    pub(super) fn keyboard_context(&self) -> (Option<Retained<NSWindow>>, bool, bool) {
        (
            self.scroll.window(),
            self.popover.isShown()
                && self.detail.is_none()
                && !self.rows.values().any(|r| r.actions.is_tracking()),
            matches!(self.detail, Some(DetailPanel::Settings(_))),
        )
    }
    pub(super) fn settings(&mut self) {
        if self.labels_saving()
            || self.keyboard_state.pending
            || matches!(self.detail, Some(DetailPanel::Settings(_)))
        {
            return;
        }
        self.show_detail(DetailPanel::settings(&self.target));
    }
    pub(super) fn shortcut(&mut self, action: HotkeyAction) -> bool {
        if !self.popover.isShown()
            || self.detail.is_some()
            || self.rows.values().any(|r| r.actions.is_tracking())
        {
            return false;
        }
        if matches!(action, HotkeyAction::Next | HotkeyAction::Previous) {
            let navigation = &mut self.navigation[usize::from(self.showing_ignored)];
            navigation.step(action == HotkeyAction::Next);
            self.pending_scroll = navigation.selected.clone();
            return true;
        }
        if action == HotkeyAction::Refresh {
            if !self.showing_ignored && self.refresh.isEnabled() {
                unsafe {
                    self.refresh.performClick(None);
                }
            }
            return false;
        }
        let Some(id) = self.navigation[usize::from(self.showing_ignored)]
            .selected
            .as_ref()
        else {
            return false;
        };
        let Some(row) = self.rows.get(id) else {
            return false;
        };
        let button = match action {
            HotkeyAction::Acknowledge if !self.showing_ignored => Some(&row.title),
            HotkeyAction::OpenPr => Some(&row.open),
            HotkeyAction::Details => Some(&row.disclosure),
            _ => None,
        };
        if let Some(button) = button
            && button.isEnabled()
        {
            // These controls retain their target and capture the displayed update.
            unsafe {
                button.performClick(None);
            }
        }
        if action == HotkeyAction::Actions
            && !self.showing_ignored
            && !row.actions.button.isHidden()
        {
            if let Some(window) = row.actions.button.window() {
                window.makeFirstResponder(Some(&row.actions.button));
            }
            unsafe {
                row.actions.button.performClick(None);
            }
        }
        false
    }
    pub(super) fn toggle_details(&mut self, id: &str) {
        let expanded = &mut self.expanded[usize::from(self.showing_ignored)];
        if !expanded.remove(id) {
            expanded.insert(id.to_owned());
        }
    }
    pub(super) fn is_showing_ignored(&self) -> bool {
        self.showing_ignored
    }
    pub(super) fn show_ignored(&mut self, ignored: bool) {
        if self.showing_ignored != ignored {
            self.scroll_positions[usize::from(self.showing_ignored)] =
                self.scroll.contentView().bounds().origin;
            self.showing_ignored = ignored;
            self.restore_scroll = true;
        }
    }
    fn show_detail(&mut self, panel: DetailPanel) {
        if let Some(previous) = self.detail.take() {
            previous.remove(&self.target);
        } else {
            self.scroll_positions[usize::from(self.showing_ignored)] =
                self.scroll.contentView().bounds().origin;
        }
        self.scroll.setDocumentView(Some(panel.view()));
        self.detail = Some(panel);
        self.scroll
            .contentView()
            .scrollToPoint(NSPoint::new(0.0, 0.0));
    }
    pub(super) fn configure_repo(&mut self, repo: &str) {
        self.show_detail(DetailPanel::configuration(repo, &self.target));
    }
    pub(super) fn show_labels(&mut self, pr: &PullRequest) {
        self.show_detail(DetailPanel::labels(pr, &self.target));
    }
    pub(super) fn label_request_handled(&mut self) {
        let pending = &self.target.ivars().pending_label_requests;
        pending.set(pending.get().saturating_sub(1));
    }
    fn labels_saving(&self) -> bool {
        self.target.ivars().pending_label_requests.get() > 0
            || self
                .detail
                .as_ref()
                .is_some_and(|detail| detail.labels_saving(&self.action_state))
    }
    pub(super) fn back(&mut self) {
        if self.labels_saving() || self.keyboard_state.pending {
            return;
        }
        if let Some(detail) = self.detail.take() {
            detail.remove(&self.target);
            self.scroll.setDocumentView(Some(&self.document));
            self.restore_scroll = true;
        } else {
            self.show_ignored(false);
        }
    }
    pub(super) fn escape(&mut self) {
        if !self.popover.isShown()
            || self.rows.values().any(|row| row.actions.is_tracking())
            || self.labels_saving()
            || self.keyboard_state.pending
        {
            return;
        }
        if self.detail.is_some() || self.showing_ignored {
            self.back();
            self.show_ignored(false);
        } else {
            self.popover.close();
        }
    }
    pub(super) fn update(
        &mut self,
        prs: &[PullRequest],
        error: Option<&str>,
        bundled: bool,
        loading: bool,
    ) {
        // Keep rows, action bindings, headings, and layout intact while a native
        // menu is tracking. menuDidClose requests an update with the latest state.
        if self.rows.values().any(|row| row.actions.is_tracking()) {
            return;
        }
        self.back
            .setEnabled(!self.labels_saving() && !self.keyboard_state.pending);
        let mtm = MainThreadMarker::new().unwrap();
        if let Some(detail) = &mut self.detail {
            if let DetailPanel::Settings(editor) = detail {
                editor.update(&self.keyboard_state);
            }
            self.refresh.setHidden(true);
            self.actions.setHidden(true);
            self.back.setHidden(false);
            self.title.setFrame(rect(20.0, 20.0, WIDTH - 140.0, 28.0));
            self.title
                .setFont(Some(&NSFont::boldSystemFontOfSize(17.0)));
            set_text(&self.title, &detail.title());
            self.title
                .setToolTip(Some(&NSString::from_str(&detail.title())));
            let message = self.action_state.error.as_deref().or(error);
            set_text(&self.summary, message.unwrap_or(detail.subtitle()));
            self.summary
                .setToolTip(message.map(NSString::from_str).as_deref());
            detail.update(prs, &self.action_state, &self.target);
            return;
        }
        self.title.setFrame(rect(20.0, 16.0, 200.0, 28.0));
        self.title
            .setFont(Some(&NSFont::boldSystemFontOfSize(21.0)));
        self.title.setToolTip(None);
        self.summary.setToolTip(None);
        let ignored = self.showing_ignored;
        self.refresh.setHidden(ignored);
        self.refresh.setEnabled(!loading);
        self.refresh.setTitle(&NSString::from_str(if loading {
            "Refreshing..."
        } else {
            "Refresh"
        }));
        self.actions.setHidden(ignored);
        self.back.setHidden(!ignored);
        set_text(&self.title, if ignored { "Ignored PRs" } else { "Gopher" });
        set_text(
            &self.empty,
            if ignored {
                "No ignored pull requests."
            } else {
                "No open pull requests. Gopher will keep checking for you."
            },
        );
        set_text(
            &self.summary,
            &if ignored {
                format!(
                    "{} ignored PRs{}",
                    prs.len(),
                    if loading {
                        " · loading details…"
                    } else {
                        ""
                    }
                )
            } else {
                format!(
                    "{} open PRs · {} updates",
                    prs.len(),
                    prs.iter().filter(|p| p.needs_attention()).count()
                )
            },
        );
        let enabled = bundled
            && unsafe {
                objc2_service_management::SMAppService::mainAppService().status()
                    == objc2_service_management::SMAppServiceStatus::Enabled
            };
        self.login.setState(if enabled {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
        let ids: HashSet<_> = prs.iter().map(|p| p.snapshot.id.as_str()).collect();
        self.rows.retain(|id, row| {
            if ids.contains(id.as_str()) {
                true
            } else {
                row.remove(&self.target);
                false
            }
        });
        let expanded = &mut self.expanded[usize::from(ignored)];
        expanded.retain(|id| ids.contains(id.as_str()));
        let repos: HashSet<_> = prs.iter().map(|p| p.snapshot.repo.as_str()).collect();
        self.headings.retain(|repo, heading| {
            if repos.contains(repo.as_str()) {
                true
            } else {
                heading.label.removeFromSuperview();
                heading.divider.removeFromSuperview();
                false
            }
        });
        let mut sorted: Vec<_> = prs.iter().collect();
        sorted.sort_by_key(|pr| {
            let (org, repo) = repo_parts(&pr.snapshot.repo);
            (org.to_lowercase(), repo.to_lowercase(), pr.snapshot.number)
        });
        self.navigation[usize::from(ignored)]
            .update(sorted.iter().map(|pr| pr.snapshot.id.clone()).collect());
        let mut y = 8.0;
        let error = self.action_state.error.as_deref().or(error);
        self.banner.setHidden(error.is_none());
        if let Some(error) = error {
            set_text(&self.banner, error);
            let height = self
                .banner
                .sizeThatFits(NSSize::new(CONTENT_WIDTH, 10000.0))
                .height;
            self.banner.setFrame(rect(16.0, y, CONTENT_WIDTH, height));
            y += height + 16.0;
        }
        let mut last_repo = "";
        for pr in sorted {
            if last_repo != pr.snapshot.repo {
                let heading = self
                    .headings
                    .entry(pr.snapshot.repo.clone())
                    .or_insert_with(|| {
                        let heading = label(&repo_heading(&pr.snapshot.repo), 12.0, true, mtm);
                        heading.setFont(Some(&NSFont::boldSystemFontOfSize(12.0)));
                        self.document.addSubview(&heading);
                        let divider = NSBox::new(mtm);
                        divider.setBoxType(NSBoxType::Separator);
                        self.document.addSubview(&divider);
                        RepositoryHeading {
                            label: heading,
                            divider,
                        }
                    });
                heading.divider.setHidden(last_repo.is_empty());
                if !last_repo.is_empty() {
                    heading
                        .divider
                        .setFrame(rect(20.0, y, CONTENT_WIDTH - 8.0, 1.0));
                    y += 8.0;
                }
                heading
                    .label
                    .setFrame(rect(20.0, y + 8.0, CONTENT_WIDTH - 8.0, 20.0));
                y += 36.0;
                last_repo = &pr.snapshot.repo;
            }
            let row = self.rows.entry(pr.snapshot.id.clone()).or_insert_with(|| {
                let row = Row::new(pr, &self.target, mtm);
                self.document.addSubview(&row.view);
                row
            });
            let height = row.update(
                pr,
                expanded.contains(&pr.snapshot.id),
                ignored,
                &self.target,
                &self.action_state,
            );
            row.background.setFrame(rect(
                -HIGHLIGHT_PADDING,
                0.0,
                CONTENT_WIDTH + 2.0 * HIGHLIGHT_PADDING,
                height - 4.0,
            ));
            row.background
                .setFillColor(&NSColor::whiteColor().colorWithAlphaComponent(0.08));
            row.background.setHidden(
                self.navigation[usize::from(ignored)].selected.as_deref()
                    != Some(pr.snapshot.id.as_str()),
            );
            row.view.setFrame(rect(
                16.0 - HIGHLIGHT_PADDING,
                y,
                CONTENT_WIDTH + 2.0 * HIGHLIGHT_PADDING,
                height,
            ));
            // Expand the drawing area while keeping controls at their existing positions.
            row.view
                .setBoundsOrigin(NSPoint::new(-HIGHLIGHT_PADDING, 0.0));
            y += height;
        }
        self.empty.setHidden(!prs.is_empty());
        self.empty
            .setFrame(rect(20.0, y + 32.0, CONTENT_WIDTH - 8.0, 54.0));
        let viewport = self.scroll.contentView().bounds();
        self.document
            .setFrameSize(NSSize::new(WIDTH, (y + 20.0).max(viewport.size.height)));
        // Preserve scroll position while clamping after removals/collapsing details.
        let clip = self.scroll.contentView();
        let mut bounds = clip.bounds();
        bounds.origin = if self.restore_scroll {
            self.scroll_positions[usize::from(ignored)]
        } else {
            viewport.origin
        };
        self.restore_scroll = false;
        let constrained = clip.constrainBoundsRect(bounds);
        clip.scrollToPoint(constrained.origin);
        self.scroll.reflectScrolledClipView(&clip);
        if let Some(id) = self.pending_scroll.take() {
            if let Some(row) = self.rows.get(&id) {
                let mut target = row.view.frame();
                // Keep the title visible even when expanded details exceed the viewport.
                target.size.height = target.size.height.min(clip.bounds().size.height);
                self.document.scrollRectToVisible(target);
                self.scroll.reflectScrolledClipView(&clip);
                tracing::info!(event="popover_pr_revealed",pr_id=%id);
            } else {
                // A delivered notification can outlive a closed, ignored, or removed PR.
                tracing::info!(event="popover_pr_unavailable",pr_id=%id);
            }
        }
    }
}
