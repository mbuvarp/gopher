//! Native, persistent review inbox. Controls retain their identity across polls;
//! actions capture the update displayed at activation, before entering the actor.
use super::{Action, AppEvent, repo_heading, repo_parts};
use crate::model::{PullRequest, State};
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained, sel,
};
use objc2_app_kit::*;
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSRectEdge, NSSize, NSString};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
};
use tao::event_loop::EventLoopProxy;
use tray_icon::TrayIcon;

const WIDTH: f64 = 580.0;
const HEIGHT: f64 = 620.0;
const CONTENT_WIDTH: f64 = WIDTH - 32.0;

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
                let _ = self.ivars().proxy.send_event(event);
            }
        }
    }
);
impl ActionTarget {
    fn new(proxy: EventLoopProxy<AppEvent>, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetState {
            proxy,
            actions: RefCell::new(HashMap::new()),
            next_tag: std::cell::Cell::new(1),
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
    view: Retained<FlippedView>,
    icon: Retained<NSImageView>,
    title: Retained<NSButton>,
    status: Retained<NSTextField>,
    open: Retained<NSButton>,
    disclosure: Retained<NSButton>,
    ignore: Retained<NSButton>,
    details: Retained<NSTextField>,
}
impl Row {
    fn new(pr: &PullRequest, target: &ActionTarget, mtm: MainThreadMarker) -> Self {
        let id = &pr.snapshot.id;
        let placeholder = AppEvent::ToggleDetails(id.clone());
        let view = FlippedView::new(rect(16.0, 0.0, CONTENT_WIDTH, 100.0), mtm);
        let icon = NSImageView::initWithFrame(NSImageView::alloc(mtm), rect(0.0, 4.0, 36.0, 48.0));
        icon.setImageScaling(NSImageScaling::ScaleProportionallyDown);
        let title = target.button("", placeholder.clone(), mtm);
        title.setBordered(false);
        title.setAlignment(NSTextAlignment::Left);
        title.setFont(Some(&NSFont::systemFontOfSize(13.0)));
        title.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        title.setFrame(rect(46.0, 4.0, CONTENT_WIDTH - 46.0, 28.0));
        let status = label("", 11.0, true, mtm);
        status.setFrame(rect(49.0, 34.0, CONTENT_WIDTH - 52.0, 18.0));
        let open = target.button("Open PR", placeholder.clone(), mtm);
        open.setFrame(rect(0.0, 57.0, 84.0, 26.0));
        let disclosure = target.button("Details", placeholder.clone(), mtm);
        disclosure.setFrame(rect(94.0, 57.0, 92.0, 26.0));
        let ignore = target.button("Ignore", placeholder, mtm);
        ignore.setFrame(rect(196.0, 57.0, 72.0, 26.0));
        let details = label("", 12.0, true, mtm);
        details.setSelectable(true);
        for child in [
            &*icon as &NSView,
            &*title,
            &*status,
            &*open,
            &*disclosure,
            &*ignore,
            &*details,
        ] {
            view.addSubview(child);
        }
        Self {
            view,
            icon,
            title,
            status,
            open,
            disclosure,
            ignore,
            details,
        }
    }
    fn update(
        &self,
        pr: &PullRequest,
        expanded: bool,
        ignored: bool,
        target: &ActionTarget,
    ) -> f64 {
        let state = if pr.stale && !ignored {
            State::Unknown
        } else {
            pr.state
        };
        self.title.setTitle(&NSString::from_str(&display_title(pr)));
        let font = if !ignored && pr.needs_attention() {
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
        set_text(
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
        );
        self.title.setEnabled(!pr.stale && !ignored);
        self.open.setEnabled(!pr.snapshot.url.is_empty());
        self.ignore.setTitle(&NSString::from_str(if ignored {
            "Restore"
        } else {
            "Ignore"
        }));
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
        if expanded {
            set_text(&self.details, &details(pr));
            let height = self
                .details
                .sizeThatFits(NSSize::new(CONTENT_WIDTH - 12.0, 10000.0))
                .height;
            self.details
                .setFrame(rect(4.0, 94.0, CONTENT_WIDTH - 12.0, height));
            106.0 + height
        } else {
            98.0
        }
    }
    fn remove(&self, target: &ActionTarget) {
        self.view.removeFromSuperview();
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
    banner: Retained<NSTextField>,
    empty: Retained<NSTextField>,
    login: Retained<NSMenuItem>,
    rows: HashMap<String, Row>,
    headings: HashMap<String, RepositoryHeading>,
    expanded: [HashSet<String>; 2],
    target: Retained<ActionTarget>,
}
impl ReviewPopover {
    pub(super) fn new(proxy: EventLoopProxy<AppEvent>, bundled: bool) -> Self {
        let mtm = MainThreadMarker::new().expect("Popover must be created on the main thread");
        let target = ActionTarget::new(proxy, mtm);
        let content = FlippedView::new(rect(0.0, 0.0, WIDTH, HEIGHT), mtm);
        let title = label("Gopher", 21.0, false, mtm);
        title.setFont(Some(&NSFont::boldSystemFontOfSize(21.0)));
        title.setFrame(rect(20.0, 16.0, 200.0, 28.0));
        content.addSubview(&title);
        let summary = label("Waiting for GitHub…", 12.0, true, mtm);
        summary.setFrame(rect(20.0, 48.0, 330.0, 20.0));
        content.addSubview(&summary);
        let refresh = target.button("Refresh", AppEvent::PopoverAction(Action::Refresh), mtm);
        refresh.setFrame(rect(WIDTH - 190.0, 24.0, 82.0, 28.0));
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
        for (title, action) in [
            ("Show ignored", Action::ShowIgnored),
            ("Edit configuration… (restart to apply)", Action::Config),
            ("Open logs…", Action::Logs),
            ("Notification settings…", Action::NotificationSettings),
            ("Launch at login", Action::Login),
            ("Quit Gopher", Action::Quit),
        ] {
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
            banner,
            empty,
            login: login.unwrap(),
            rows: HashMap::new(),
            headings: HashMap::new(),
            expanded: Default::default(),
            target,
        }
    }
    pub(super) fn toggle(&self, tray: &TrayIcon) {
        if self.popover.isShown() {
            self.popover.close();
            return;
        }
        if let Some(item) = tray.ns_status_item()
            && let Some(button) = item.button(MainThreadMarker::new().unwrap())
        {
            self.popover.showRelativeToRect_ofView_preferredEdge(
                button.bounds(),
                &button,
                NSRectEdge::MinY,
            );
            if let Some(window) = self.document.window() {
                window.makeKeyWindow();
            }
            tracing::debug!(event = "popover_opened");
        }
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
    pub(super) fn update(
        &mut self,
        prs: &[PullRequest],
        error: Option<&str>,
        bundled: bool,
        loading: bool,
    ) {
        let mtm = MainThreadMarker::new().unwrap();
        let ignored = self.showing_ignored;
        self.refresh.setHidden(ignored);
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
        let mut y = 8.0;
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
            );
            row.view.setFrame(rect(16.0, y, CONTENT_WIDTH, height));
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
    }
}
