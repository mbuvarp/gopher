use crate::{
    config::Config,
    model::*,
    worker::{self, Command, UiEvent},
};
use anyhow::{Context, Result};
use block2::{DynBlock, RcBlock};
use objc2::{
    AnyThread, DefinedClass, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, Bool, ProtocolObject},
    sel,
};
use objc2_app_kit::{
    NSImage, NSMenu, NSMenuDidBeginTrackingNotification, NSMenuDidEndTrackingNotification,
    NSMenuItem, NSWorkspace,
};
use objc2_foundation::{
    NSArray, NSBundle, NSError, NSNotification, NSNotificationCenter, NSObject, NSObjectProtocol,
    NSSet, NSSize, NSString, NSURL,
};
use objc2_service_management::{SMAppService, SMAppServiceStatus};
use objc2_user_notifications::*;
use std::{
    collections::HashMap,
    path::PathBuf,
    ptr::NonNull,
    sync::{Arc, Mutex, OnceLock},
};
use tao::{
    event::{Event, StartCause},
    event_loop::{ControlFlow, EventLoopBuilder},
    platform::{
        macos::{ActivationPolicy, EventLoopExtMacOS},
        run_return::EventLoopExtRunReturn,
    },
};
use tokio::sync::mpsc::UnboundedSender;
use tray_icon::{
    MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{CheckMenuItem, ContextMenu, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
};

mod keyboard;
mod popover;
mod updates;
pub use updates::finish_native_termination;

#[derive(Clone, Debug)]
enum AppEvent {
    Worker(UiEvent),
    UpdaterChanged,
    UpdateEdit(updates::Edit),
    Shortcut(crate::hotkeys::HotkeyAction),
    Escape,
    GlobalShortcut(u32),
    HotkeyEdit(keyboard::Edit),
    ShortcutRecorded(crate::hotkeys::HotkeyAction, crate::hotkeys::Binding),
    HighlightPr(String),
    Menu(String),
    MenuClosed,
    Permission(bool),
    NotificationError(String),
    TogglePopover,
    PopoverAction(Action),
    ToggleDetails(String),
    PrAction(crate::actions::Request),
    RefreshPopover,
    Shutdown(&'static str),
}

const REVIEW_CATEGORY: &str = "gopher.review";
const ACKNOWLEDGE_ACTION: &str = "gopher.acknowledge";
const OPEN_PR_ACTION: &str = "gopher.open-pr";

fn notification_command(action: &str, id: String) -> Option<Command> {
    let open = match action {
        "com.apple.UNNotificationDefaultActionIdentifier" | ACKNOWLEDGE_ACTION => false,
        OPEN_PR_ACTION => true,
        _ => return None,
    };
    Some(Command::NotificationAction {
        id,
        open,
        reveal: action == "com.apple.UNNotificationDefaultActionIdentifier",
    })
}

fn review_category() -> Retained<UNNotificationCategory> {
    let acknowledge = UNNotificationAction::actionWithIdentifier_title_options(
        &NSString::from_str(ACKNOWLEDGE_ACTION),
        &NSString::from_str("Acknowledge"),
        UNNotificationActionOptions::empty(),
    );
    let open = UNNotificationAction::actionWithIdentifier_title_options(
        &NSString::from_str(OPEN_PR_ACTION),
        &NSString::from_str("Open PR"),
        UNNotificationActionOptions::Foreground,
    );
    UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
        &NSString::from_str(REVIEW_CATEGORY),
        &NSArray::from_retained_slice(&[acknowledge, open]),
        &NSArray::new(),
        UNNotificationCategoryOptions::empty(),
    )
}

// Notification callbacks may arrive off the main thread; only send actor messages here.
define_class!(
    #[unsafe(super = NSObject)]
    #[ivars = UnboundedSender<Command>]
    struct NotificationDelegate;
    unsafe impl NSObjectProtocol for NotificationDelegate {}
    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(&self, _center: &UNUserNotificationCenter, response: &UNNotificationResponse, completion: &DynBlock<dyn Fn()>) {
            let action=response.actionIdentifier().to_string();
            let id=response.notification().request().identifier().to_string();
            if let Some(command) = notification_command(&action, id) {
                let _=self.ivars().send(command);
            }
            completion.call(());
        }
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(&self, _center: &UNUserNotificationCenter, _notification: &UNNotification, completion: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>) {
            completion.call((UNNotificationPresentationOptions::Banner | UNNotificationPresentationOptions::List | UNNotificationPresentationOptions::Sound,));
        }
    }
);
impl NotificationDelegate {
    fn new(sender: UnboundedSender<Command>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(sender);
        // NSObject initialization with its documented selector and signature.
        unsafe { msg_send![super(this), init] }
    }
}

// A submenu row can also be selected when it has an explicit target/action.
// Keep one target alive for the whole event loop; menu items do not retain it.
define_class!(
    #[unsafe(super = NSObject)]
    #[ivars = tao::event_loop::EventLoopProxy<AppEvent>]
    struct PrMenuTarget;
    unsafe impl NSObjectProtocol for PrMenuTarget {}
    impl PrMenuTarget {
        #[unsafe(method(acknowledgePr:))]
        fn acknowledge_pr(&self, item: &NSMenuItem) {
            if let Some(object) = item.representedObject()
                && let Some(id) = object.downcast_ref::<NSString>() {
                let _ = self.ivars().send_event(AppEvent::Menu(id.to_string()));
            }
        }
    }
);
impl PrMenuTarget {
    fn new(proxy: tao::event_loop::EventLoopProxy<AppEvent>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(proxy);
        // Initialize the NSObject superclass before it becomes a menu target.
        unsafe { msg_send![super(this), init] }
    }
}

#[derive(Clone, Debug)]
enum Action {
    Open {
        url: String,
        pr: String,
        update: String,
    },
    Ignore(String),
    Restore(String),
    ShowIgnored,
    BackToActive,
    ConfigureRepo(String),
    Labels(String),
    Ack {
        pr: String,
        update: String,
        checked: bool,
    },
    Refresh,
    Config,
    ConfigFile,
    Logs,
    Login,
    NotificationSettings,
    CheckUpdates,
    Quit,
}

// Keep the menu and its action payloads stable throughout native menu tracking.
#[derive(Default)]
struct MenuState {
    tracking: bool,
    dirty: bool,
    actions: HashMap<String, Action>,
    shown_actions: HashMap<String, Action>,
}
impl MenuState {
    fn begin_tracking(&mut self) {
        self.tracking = true;
        self.shown_actions = self.actions.clone();
    }
    fn request_update(&mut self) {
        if self.tracking && !self.dirty {
            tracing::debug!(event = "menu_update_deferred");
        }
        self.dirty = true;
    }
    fn can_update(&self) -> bool {
        self.dirty && !self.tracking
    }
    fn installed(&mut self, actions: HashMap<String, Action>) {
        self.actions = actions;
        self.dirty = false;
    }
    fn action(&self, id: &str) -> Option<Action> {
        // AppKit may end tracking before dispatching the selected item's action.
        // Preserve the last displayed update IDs even if a refresh arrives first.
        self.shown_actions
            .get(id)
            .or_else(|| self.actions.get(id))
            .cloned()
    }
}

struct MenuObserver {
    center: Retained<NSNotificationCenter>,
    tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}
impl MenuObserver {
    fn new(
        menu: &Menu,
        state: Arc<Mutex<MenuState>>,
        proxy: tao::event_loop::EventLoopProxy<AppEvent>,
    ) -> Self {
        let center = NSNotificationCenter::defaultCenter();
        let mut tokens = Vec::new();
        // muda owns this NSMenu; it remains alive while installed in the tray.
        // Observe only the root menu, so closing a submenu cannot release the guard.
        let object = unsafe { &*menu.ns_menu().cast::<AnyObject>() };
        for tracking in [true, false] {
            let state = state.clone();
            let proxy = proxy.clone();
            let callback = RcBlock::new(move |_notification: NonNull<NSNotification>| {
                {
                    let mut state = state.lock().unwrap();
                    if tracking {
                        state.begin_tracking();
                    } else {
                        state.tracking = false;
                    }
                }
                tracing::debug!(event = "menu_tracking", open = tracking);
                if !tracking {
                    let _ = proxy.send_event(AppEvent::MenuClosed);
                }
            });
            // AppKit posts tracking notifications synchronously on the main thread.
            // A nil queue updates the guard immediately, before queued worker events.
            // The block captures only sendable state and an event-loop proxy.
            let token = unsafe {
                center.addObserverForName_object_queue_usingBlock(
                    Some(if tracking {
                        NSMenuDidBeginTrackingNotification
                    } else {
                        NSMenuDidEndTrackingNotification
                    }),
                    Some(object),
                    None,
                    &callback,
                )
            };
            tokens.push(token);
        }
        Self { center, tokens }
    }
}
impl Drop for MenuObserver {
    fn drop(&mut self) {
        for token in &self.tokens {
            // These are the observer tokens returned by this notification center.
            unsafe {
                self.center.removeObserver((**token).as_ref());
            }
        }
    }
}

pub fn run(
    directory: PathBuf,
    config: Config,
    log: crate::logging::LogWriter,
) -> Result<&'static str> {
    let mut event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    event_loop.set_dock_visibility(false);
    event_loop.set_activate_ignoring_other_apps(false);
    let signal_proxy = event_loop.create_proxy();
    let signal_log = log.clone();
    let _signals = crate::lifecycle::ShutdownSignals::start(move |reason| {
        if let Err(error) = signal_log.diagnostic(
            "INFO",
            serde_json::json!({
                "event": "shutdown_requested", "reason": reason, "pid": std::process::id(),
            }),
        ) {
            eprintln!("Gopher could not log termination signal: {error}");
        }
        let _ = signal_proxy.send_event(AppEvent::Shutdown(reason));
    })?;
    let proxy = event_loop.create_proxy();
    let sink = Arc::new(move |event| {
        let _ = proxy.send_event(AppEvent::Worker(event));
    });
    let worker = worker::start(directory.clone(), config, sink)?;
    let sender = worker.sender.clone();
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = proxy.send_event(AppEvent::Menu(event.id.0));
    }));
    let proxy = event_loop.create_proxy();
    let bundled = NSBundle::mainBundle().bundleIdentifier().is_some();
    let mut center = None;
    let mut delegate = None;
    let mut permission = None;
    if bundled {
        let notification_center = UNUserNotificationCenter::currentNotificationCenter();
        notification_center
            .setNotificationCategories(&NSSet::from_retained_slice(&[review_category()]));
        let notification_delegate = NotificationDelegate::new(sender.clone());
        notification_center.setDelegate(Some(ProtocolObject::from_ref(&*notification_delegate)));
        let completion = RcBlock::new(move |granted: Bool, error: *mut NSError| {
            if !error.is_null() {
                // The framework owns this NSError for the callback's duration.
                let error = unsafe { &*error };
                tracing::warn!(event = "notification_permission_error",code=error.code(),domain=%error.domain(),description=%error.localizedDescription());
            }
            let _ = proxy.send_event(AppEvent::Permission(granted.as_bool()));
        });
        notification_center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
            &completion,
        );
        center = Some(notification_center);
        delegate = Some(notification_delegate);
    }
    let pr_menu_target = PrMenuTarget::new(event_loop.create_proxy());
    let mut popover =
        popover::ReviewPopover::new(event_loop.create_proxy(), bundled, sender.clone());
    let mut keyboard = keyboard::Keyboard::new(event_loop.create_proxy());
    let tray_proxy = event_loop.create_proxy();
    TrayIconEvent::set_event_handler(Some(move |event| {
        if matches!(
            event,
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
        ) {
            let _ = tray_proxy.send_event(AppEvent::TogglePopover);
        }
    }));
    let mut tray: Option<TrayIcon> = None;
    let menu_state = Arc::new(Mutex::new(MenuState::default()));
    let mut menu_observer = None;
    let mut prs = Vec::new();
    let mut ignored_prs = Vec::new();
    let mut ignored_error = None;
    let mut ignored_loading = false;
    let mut refreshing = false;
    let mut service_error = None;
    let mut ui_error = if bundled {
        None
    } else {
        Some("Notifications require the packaged Gopher.app; run scripts/bundle.sh.".to_owned())
    };
    let mut pending = Vec::new();
    let proxy = event_loop.create_proxy();
    let updater = updates::Updater::new(proxy.clone());
    let mut shutting_down = false;
    let mut worker_stopped = false;
    let mut exit_reason = "event_loop_returned";
    let mut startup_error = None;
    let mut pending_reveal: Option<Option<String>> = None;
    event_loop.run_return(|event,_,flow| {
        *flow=ControlFlow::Wait;
        let mut rebuild=false;
        if shutting_down && !matches!(&event, Event::UserEvent(AppEvent::Worker(_)) | Event::LoopDestroyed | Event::UserEvent(AppEvent::Shutdown(_))) {
            return;
        }
        match event {
            Event::UserEvent(AppEvent::UpdaterChanged) => { rebuild=true; }
            Event::UserEvent(AppEvent::UpdateEdit(edit)) => { updater.edit(edit); rebuild=true; }
            Event::UserEvent(AppEvent::Worker(UiEvent::Stopped)) => {
                worker_stopped = true;
                if shutting_down { *flow = ControlFlow::Exit; }
                else { ui_error=Some("Gopher’s background worker stopped. Restart Gopher.".into()); rebuild=true; }
            }
            Event::LoopDestroyed => {
                if let Err(error)=log.diagnostic("INFO",serde_json::json!({"event":"event_loop_stopped","reason":exit_reason,"pid":std::process::id()})) {eprintln!("Gopher could not log event loop shutdown: {error}");}
            }
            Event::UserEvent(AppEvent::Shutdown(reason)) => {
                exit_reason = reason;
                shutting_down = true;
                let _ = sender.send(Command::Shutdown);
                if worker_stopped { *flow = ControlFlow::Exit; }
            }
            Event::NewEvents(StartCause::Init) => {
                match TrayIconBuilder::new().with_menu_on_left_click(false).with_tooltip("Gopher — GitHub reviews").with_icon(icon(MenuBarState::Idle)).with_icon_as_template(true).build() {
                    Ok(icon)=>{tray=Some(icon);tracing::info!(event="menu_bar_created");},
                    Err(e)=>{startup_error=Some(anyhow::anyhow!("Cannot create menu bar icon: {e}"));*flow=ControlFlow::Exit;}
                }
                rebuild=true;
            }
            Event::UserEvent(AppEvent::Permission(granted)) => {
                permission=Some(granted);
                if !granted {ui_error=Some("Notifications are disabled. Enable Gopher in System Settings → Notifications.".into());}
                else if ui_error.as_deref().is_some_and(|e|e.starts_with("Notifications are disabled")){ui_error=None;}
                tracing::info!(event="notification_permission",granted);
                if granted { for (id,title,body,review) in pending.drain(..) {
                    if let Some(center)=&center {notify(center,id,title,body,review,sender.clone(),proxy.clone());}
                }} else {for (id,_,_,_) in pending.drain(..){let _=sender.send(Command::NotificationFailed(id));}}
                rebuild=true;
            }
            Event::UserEvent(AppEvent::NotificationError(message)) => {ui_error=Some(message);rebuild=true;}
            Event::UserEvent(AppEvent::Worker(event)) => match event {
                UiEvent::Stopped => unreachable!(),
                UiEvent::HotkeysLoaded {preferences,error}=>{keyboard.load(preferences,error);rebuild=true;}
                UiEvent::HotkeysSaved(result)=>{keyboard.saved(result);rebuild=true;}
                UiEvent::ActionsChanged(state)=>{popover.action_state=state;rebuild=true;}
                UiEvent::LabelRequestHandled=>{popover.label_request_handled();rebuild=true;}
                UiEvent::IgnoredUpdated{prs:updated,error,loading}=>{ignored_prs=updated;ignored_error=error;ignored_loading=loading;rebuild=true;}
                UiEvent::Updated{prs:updated,error,loading}=>{prs=updated;service_error=error;refreshing=loading;rebuild=true;}
                UiEvent::ShowPopover{pr}=>{
                    pending_reveal=Some(pr);
                    if let Some(tray)=&tray {popover.show(tray);}
                    rebuild=true;
                }
                UiEvent::Open{url,pr,update}=>{match open_and_acknowledge(&url,&pr,&update,&sender,open_url){
                    Ok(())=>{}
                    Err(e)=>{ui_error=Some(e.to_string());rebuild=true;}
                }}
                UiEvent::DismissNotifications(ids)=>{
                    pending.retain(|(id,_,_,_)|{
                        if ids.contains(id) {let _=sender.send(Command::NotificationFailed(id.clone()));false} else {true}
                    });
                    if let Some(center)=&center {
                        let ids=NSArray::from_retained_slice(&ids.iter().map(|id|NSString::from_str(id)).collect::<Vec<_>>());
                        center.removePendingNotificationRequestsWithIdentifiers(&ids);
                        center.removeDeliveredNotificationsWithIdentifiers(&ids);
                    }
                }
                UiEvent::Notify{id,title,body,review}=>{
                    if permission==Some(true) {
                        if let Some(center)=&center {notify(center,id,title,body,review,sender.clone(),proxy.clone());}
                    } else if permission.is_none() && bundled {pending.push((id,title,body,review));}
                    else {let _=sender.send(Command::NotificationFailed(id));}
                }
            },
            Event::UserEvent(AppEvent::Shortcut(action)) => {rebuild=popover.shortcut(action);}
            Event::UserEvent(AppEvent::Escape) => {popover.escape();rebuild=true;}
            Event::UserEvent(AppEvent::GlobalShortcut(id)) => {
                if keyboard.global_event(id,&sender) && let Some(tray)=&tray {popover.toggle(tray);}
                rebuild=true;
            }
            Event::UserEvent(AppEvent::HotkeyEdit(edit)) => {keyboard.edit(edit,&sender);rebuild=true;}
            Event::UserEvent(AppEvent::ShortcutRecorded(action,binding)) => {keyboard.recorded(action,Some(binding),&sender);rebuild=true;}
            Event::UserEvent(AppEvent::HighlightPr(id)) => {popover.highlight(id);rebuild=true;}
            Event::UserEvent(AppEvent::MenuClosed) => {}
            Event::UserEvent(AppEvent::RefreshPopover) => {rebuild=true;}
            Event::UserEvent(AppEvent::PrAction(request)) => {let _=sender.send(Command::PrAction(crate::worker::ActionCommand::Request(request)));}
            Event::UserEvent(AppEvent::TogglePopover) => {
                if let Some(tray) = &tray { popover.toggle(tray); }
                rebuild=true;
            }
            Event::UserEvent(AppEvent::ToggleDetails(id)) => {
                popover.toggle_details(&id);
                rebuild = true;
            }
            Event::UserEvent(event @ (AppEvent::Menu(_) | AppEvent::PopoverAction(_))) => {
                let action = match event {
                    AppEvent::Menu(id) => menu_state.lock().unwrap().action(&id),
                    AppEvent::PopoverAction(action) => Some(action),
                    _ => unreachable!(),
                };
                if let Some(action)=action.as_ref() {
                    let result: Result<()> = (|| {
                        match action {
                            Action::Open{url,pr,update}=>open_and_acknowledge(url,pr,update,&sender,open_url)?,
                            Action::Ignore(pr)=>{let _=sender.send(Command::Ignore(pr.clone()));}
                            Action::Restore(pr)=>{let _=sender.send(Command::Restore(pr.clone()));}
                            Action::ShowIgnored=>{popover.show_ignored(true);let _=sender.send(Command::ShowIgnored);rebuild=true;}
                            Action::BackToActive=>{popover.back();rebuild=true;}
                            Action::ConfigureRepo(repo)=>{popover.configure_repo(repo);rebuild=true;}
                            Action::Labels(id)=>{
                                if let Some(pr)=prs.iter().find(|pr|&pr.snapshot.id==id) {
                                    popover.show_labels(pr);
                                    rebuild=true;
                                }
                            }
                            Action::Ack{pr,update,checked}=>{let _=sender.send(Command::Acknowledge{pr:pr.clone(),update:update.clone(),checked:*checked});}
                            Action::Refresh=>{
                                let _=sender.send(Command::Refresh);
                                if let Some(center)=&center {
                                    let proxy=proxy.clone();
                                    let callback=RcBlock::new(move |granted:Bool,_error:*mut NSError|{let _=proxy.send_event(AppEvent::Permission(granted.as_bool()));});
                                    center.requestAuthorizationWithOptions_completionHandler(UNAuthorizationOptions::Alert|UNAuthorizationOptions::Sound,&callback);
                                }
                            }
                            Action::Config=>{
                                popover.settings();
                                if let Some(tray)=&tray {popover.show(tray);}
                                rebuild=true;
                            }
                            Action::ConfigFile=>{
                                let path=directory.join("config.toml");
                                // File work is dispatched off the menu callback.
                                let proxy=proxy.clone();
                                std::thread::spawn(move || {
                                    if !path.exists()&& let Err(e)=std::fs::write(&path,include_str!("../config.example.toml")) {let _=proxy.send_event(AppEvent::NotificationError(e.to_string()));return;}
                                    let _=std::process::Command::new("/usr/bin/open").arg("-t").arg(path).status();
                                });
                            }
                            Action::Logs=>{let url=NSURL::fileURLWithPath(&NSString::from_str(&directory.join("logs").to_string_lossy()));NSWorkspace::sharedWorkspace().openURL(&url);}
                            Action::NotificationSettings=>{let url=NSURL::URLWithString(&NSString::from_str("x-apple.systempreferences:com.apple.Notifications-Settings.extension")).context("Invalid settings URL")?;NSWorkspace::sharedWorkspace().openURL(&url);}
                            Action::Login=>{
                                // SMAppService operates on the signed main bundle in the current user session.
                                unsafe {
                                    let service=SMAppService::mainAppService();
                                    if service.status()==SMAppServiceStatus::Enabled {service.unregisterAndReturnError().map_err(|e|anyhow::anyhow!(e.to_string()))?;}
                                    else {service.registerAndReturnError().map_err(|e|anyhow::anyhow!(e.to_string()))?;}
                                    if service.status()==SMAppServiceStatus::RequiresApproval {SMAppService::openSystemSettingsLoginItems();}
                                }
                                rebuild=true;
                            }
                            Action::CheckUpdates=>{updater.check();}
                            Action::Quit=>{
                                exit_reason="quit";
                                if let Err(error)=log.diagnostic("INFO",serde_json::json!({"event":"shutdown_requested","reason":"quit","pid":std::process::id()})) {eprintln!("Gopher could not log quit: {error}");}
                                shutting_down=true;let _=sender.send(Command::Shutdown);if worker_stopped {*flow=ControlFlow::Exit;}
                            }
                        }
                        Ok(())
                    })();
                    if let Err(e)=result {tracing::error!(event="menu_action_failed",error=%e);ui_error=Some(e.to_string());rebuild=true;}
                }
            }
            _=>(),
        }
        popover.update_state = updater.state();
        let (window,inbox,settings)=popover.keyboard_context();
        keyboard.sync(window,inbox,settings);
        popover.keyboard_state=keyboard.state.borrow().clone();
        if pending_reveal.is_some() && tray.is_some() && popover.prepare_notification_reveal() {
            if let Some(tray)=&tray {popover.show(tray);}
            popover.scroll_to_pr(pending_reveal.take().flatten());
            rebuild=true;
        }
        if rebuild {
            if popover.is_showing_ignored() {
                popover.update(&ignored_prs, ignored_error.as_deref(), bundled, ignored_loading);
            } else {
                popover.update(&prs, service_error.as_deref().or(ui_error.as_deref()), bundled, refreshing);
            }
        }
        let (window,inbox,settings)=popover.keyboard_context();
        keyboard.sync(window,inbox,settings);
        if let Some(next)=popover.animate() && !matches!(*flow,ControlFlow::Exit) {
            *flow=ControlFlow::WaitUntil(next);
        }
        let update_menu = {
            let mut state = menu_state.lock().unwrap();
            if rebuild { state.request_update(); }
            state.can_update()
        };
        if update_menu
            && let Some(tray)=&tray {
                let error=service_error.as_deref().or(ui_error.as_deref());
                match menu(&prs,error,bundled,&pr_menu_target,&updater.state()) {
                    Ok((menu,new_actions))=>{
                        menu_observer=Some(MenuObserver::new(&menu,menu_state.clone(),proxy.clone()));
                        tray.set_menu(Some(Box::new(menu)));
                        menu_state.lock().unwrap().installed(new_actions);
                    }
                    Err(e)=>tracing::error!(event="menu_build_failed",error=%e),
                }
                let attention:Vec<_>=prs.iter().filter(|p|p.needs_attention()).collect();
                let state=menu_bar_state(&prs,error.is_some());
                let _=tray.set_icon_with_as_template(Some(icon(state)),true);
                // On macOS, None leaves the existing status-item title unchanged.
                tray.set_title(Some(if error.is_some(){"!"}else{""}));
                let _=tray.set_tooltip(Some(format!("Gopher · {} PRs · {} updates",prs.len(),attention.len())));
                tracing::debug!(event="menu_updated",prs=prs.len(),updates=attention.len(),state=?state);
            }
    });
    let _ = sender.send(Command::Shutdown);
    drop(menu_observer);
    drop(delegate);
    match startup_error {
        Some(error) => Err(error),
        None => Ok(exit_reason),
    }
}

fn notify(
    center: &UNUserNotificationCenter,
    id: String,
    title: String,
    body: String,
    review: bool,
    sender: UnboundedSender<Command>,
    proxy: tao::event_loop::EventLoopProxy<AppEvent>,
) {
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(&title));
    content.setBody(&NSString::from_str(&body));
    content.setSound(Some(&UNNotificationSound::defaultSound()));
    if review {
        content.setCategoryIdentifier(&NSString::from_str(REVIEW_CATEGORY));
    }
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(&id),
        &content,
        None,
    );
    let completion = RcBlock::new(move |error: *mut NSError| {
        if error.is_null() {
            tracing::info!(event="notification_scheduled",notification=%id);
            let _ = sender.send(Command::NotificationDelivered(id.clone()));
        } else {
            tracing::warn!(event="notification_failed",notification=%id);
            let _ = sender.send(Command::NotificationFailed(id.clone()));
            let _ = proxy.send_event(AppEvent::NotificationError(
                "macOS could not schedule a notification. Check notification permissions.".into(),
            ));
        }
    });
    center.addNotificationRequest_withCompletionHandler(&request, Some(&completion));
}

fn open_url(url: &str) -> Result<()> {
    let parsed = url::Url::parse(url)?;
    anyhow::ensure!(
        parsed.scheme() == "https"
            && parsed.host_str() == Some("github.com")
            && parsed.username().is_empty()
            && parsed.password().is_none(),
        "Refusing an invalid GitHub PR URL"
    );
    let url = NSURL::URLWithString(&NSString::from_str(url)).context("Invalid PR URL")?;
    anyhow::ensure!(
        NSWorkspace::sharedWorkspace().openURL(&url),
        "Could not open the PR in your browser"
    );
    Ok(())
}

fn open_and_acknowledge(
    url: &str,
    pr: &str,
    update: &str,
    sender: &UnboundedSender<Command>,
    open: impl FnOnce(&str) -> Result<()>,
) -> Result<()> {
    open(url)?;
    sender
        .send(Command::Acknowledge {
            pr: pr.into(),
            update: update.into(),
            checked: true,
        })
        .map_err(|_| anyhow::anyhow!("PR opened, but Gopher could not acknowledge the update"))?;
    Ok(())
}

fn repo_parts(repo: &str) -> (&str, &str) {
    repo.split_once('/').unwrap_or(("", repo))
}

fn repo_heading(repo: &str) -> String {
    let (organization, repository) = repo_parts(repo);
    if organization.is_empty() {
        repository.to_owned()
    } else {
        format!("{repository} • {organization}")
    }
}

fn append_pr_submenu(
    menu: &Menu,
    submenu: &Submenu,
    state: State,
    target: &PrMenuTarget,
) -> Result<()> {
    let symbol = match state {
        State::Unknown => "questionmark.circle",
        State::Reviewing => "arrow.triangle.2.circlepath",
        State::Comments => "text.bubble",
        State::Approved => "checkmark.circle",
    };
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(symbol),
        Some(&NSString::from_str(state.label())),
    )
    .with_context(|| format!("Could not load menu icon {symbol}"))?;
    image.setSize(NSSize::new(16.0, 16.0));
    image.setTemplate(true);
    menu.append(submenu)?;
    // muda exposes the parent NSMenu but only a fixed list of legacy native icons.
    // Attach the SF Symbol to the newly appended NSMenuItem. The menu owns this
    // pointer, and menu construction runs exclusively on the main thread.
    let native_menu = unsafe { &*menu.ns_menu().cast::<NSMenu>() };
    let item = native_menu
        .itemAtIndex(native_menu.numberOfItems() - 1)
        .context("Missing newly appended PR menu item")?;
    item.setImage(Some(&image));
    // Setting this after the submenu is attached preserves hover navigation while
    // making a direct selection dispatch our command. The target outlives the tray.
    unsafe {
        // Our action reads this represented object as an NSString menu identifier.
        item.setRepresentedObject(Some(&NSString::from_str(&submenu.id().0)));
        item.setTarget(Some(target));
        item.setAction(Some(sel!(acknowledgePr:)));
    }
    Ok(())
}

fn menu(
    prs: &[PullRequest],
    error: Option<&str>,
    bundled: bool,
    pr_menu_target: &PrMenuTarget,
    update_state: &updates::State,
) -> Result<(Menu, HashMap<String, Action>)> {
    let menu = Menu::new();
    let mut actions = HashMap::new();
    menu.append(&MenuItem::new(
        format!("Gopher · {} open PRs", prs.len()),
        false,
        None,
    ))?;
    if let Some(error) = error {
        let details = Submenu::new("⚠ Gopher needs attention", true);
        for chunk in error.as_bytes().chunks(100) {
            details.append(&MenuItem::new(String::from_utf8_lossy(chunk), false, None))?;
        }
        menu.append(&details)?;
    }
    let mut sorted: Vec<_> = prs.iter().collect();
    sorted.sort_by_key(|p| {
        let (organization, repository) = repo_parts(&p.snapshot.repo);
        (
            organization.to_lowercase(),
            repository.to_lowercase(),
            p.snapshot.number,
        )
    });
    let mut last_repo = "";
    for pr in sorted {
        if pr.snapshot.repo != last_repo {
            menu.append(&PredefinedMenuItem::separator())?;
            menu.append(&MenuItem::new(repo_heading(&pr.snapshot.repo), false, None))?;
            last_repo = &pr.snapshot.repo;
        }
        let state = if pr.stale { State::Unknown } else { pr.state };
        let title: String = pr
            .snapshot
            .title
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(75)
            .collect();
        let item = Submenu::new(
            format!(
                "{}#{} {}",
                if pr.needs_attention() { "● " } else { "" },
                pr.snapshot.number,
                title,
            ),
            true,
        );
        actions.insert(
            item.id().0.clone(),
            Action::Ack {
                pr: pr.snapshot.id.clone(),
                update: pr.update_id.clone(),
                checked: true,
            },
        );
        let open = MenuItem::new("Open PR", true, None);
        actions.insert(
            open.id().0.clone(),
            Action::Open {
                url: pr.snapshot.url.clone(),
                pr: pr.snapshot.id.clone(),
                update: pr.update_id.clone(),
            },
        );
        item.append(&open)?;
        let checked = pr.acknowledged.as_deref() == Some(&pr.update_id);
        let ack = CheckMenuItem::new("Acknowledge update", !pr.stale, checked, None);
        actions.insert(
            ack.id().0.clone(),
            Action::Ack {
                pr: pr.snapshot.id.clone(),
                update: pr.update_id.clone(),
                checked: !checked,
            },
        );
        item.append(&ack)?;
        let ignore = MenuItem::new("Ignore PR", true, None);
        actions.insert(
            ignore.id().0.clone(),
            Action::Ignore(pr.snapshot.id.clone()),
        );
        item.append(&ignore)?;
        item.append(&PredefinedMenuItem::separator())?;
        item.append(&MenuItem::new(
            format!(
                "{}{} · {} unresolved threads",
                pr.status_label(chrono::Utc::now().timestamp()),
                if pr.stale { " (cached)" } else { "" },
                pr.snapshot.threads.iter().filter(|t| !t.resolved).count()
            ),
            false,
            None,
        ))?;
        if pr.snapshot.draft {
            item.append(&MenuItem::new("Draft pull request", false, None))?;
        }
        for agent in &pr.agents {
            item.append(&MenuItem::new(
                format!("{}: {:?}", agent.agent.label(), agent.verdict),
                false,
                None,
            ))?;
            item.append(&MenuItem::new(&agent.reason, false, None))?;
        }
        if pr.agents.is_empty() {
            item.append(&MenuItem::new(
                "No agent review activity detected",
                false,
                None,
            ))?;
        }
        if let Some(error) = &pr.error {
            item.append(&MenuItem::new(error, false, None))?;
        }
        if let Some(time) = chrono::DateTime::from_timestamp(pr.fetched_at, 0) {
            item.append(&MenuItem::new(
                format!(
                    "Last fetched {}",
                    time.with_timezone(&chrono::Local).format("%H:%M:%S")
                ),
                false,
                None,
            ))?;
        }
        append_pr_submenu(&menu, &item, state, pr_menu_target)?;
    }
    menu.append(&PredefinedMenuItem::separator())?;
    let actions_menu = Submenu::new("Actions", true);
    for (label, action) in [
        ("Refresh now", Action::Refresh),
        ("Settings…", Action::Config),
        ("Open logs…", Action::Logs),
        ("Notification settings…", Action::NotificationSettings),
    ] {
        let item = MenuItem::new(label, true, None);
        actions.insert(item.id().0.clone(), action);
        actions_menu.append(&item)?;
    }
    let enabled = bundled
        && unsafe { SMAppService::mainAppService().status() == SMAppServiceStatus::Enabled };
    let login = CheckMenuItem::new("Launch at login", bundled, enabled, None);
    actions.insert(login.id().0.clone(), Action::Login);
    actions_menu.append(&login)?;
    menu.append(&actions_menu)?;
    let update = MenuItem::new(update_state.menu_title(), update_state.can_check, None);
    actions.insert(update.id().0.clone(), Action::CheckUpdates);
    menu.append(&update)?;
    let quit = MenuItem::new("Quit Gopher", true, None);
    actions.insert(quit.id().0.clone(), Action::Quit);
    menu.append(&quit)?;
    Ok((menu, actions))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuBarState {
    Idle,
    Review(State),
}

fn menu_bar_state(prs: &[PullRequest], has_error: bool) -> MenuBarState {
    if has_error {
        return MenuBarState::Review(State::Unknown);
    }
    for state in [State::Comments, State::Approved] {
        if prs
            .iter()
            .any(|pr| pr.needs_attention() && pr.state == state)
        {
            return MenuBarState::Review(state);
        }
    }
    let unacknowledged = |pr: &&PullRequest| pr.acknowledged.as_deref() != Some(&pr.update_id);
    if prs
        .iter()
        .filter(unacknowledged)
        .any(|pr| !pr.stale && pr.state == State::Reviewing)
    {
        MenuBarState::Review(State::Reviewing)
    } else if prs
        .iter()
        .filter(unacknowledged)
        .any(|pr| pr.stale || pr.state == State::Unknown)
    {
        MenuBarState::Review(State::Unknown)
    } else {
        MenuBarState::Idle
    }
}

fn gopher_rgba() -> &'static [u8] {
    static PIXELS: OnceLock<Vec<u8>> = OnceLock::new();
    PIXELS.get_or_init(|| template_rgba(include_bytes!("../assets/gopher.png")))
}

fn template_rgba(bytes: &[u8]) -> Vec<u8> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("bundled gopher PNG");
    let mut pixels = vec![0; reader.output_buffer_size().expect("bounded icon size")];
    let info = reader
        .next_frame(&mut pixels)
        .expect("valid gopher artwork");
    assert_eq!(info.color_type, png::ColorType::Rgba);
    assert_eq!(info.bit_depth, png::BitDepth::Eight);
    let (width, height) = (info.width as usize, info.height as usize);
    let mut rgba = vec![0; 36 * 36 * 4];
    // Average the alpha mask into a Retina-sized template icon once. AppKit
    // supplies the ink color for the current menu bar appearance.
    for y in 0..36 {
        for x in 0..36 {
            let mut alpha = 0_u64;
            let mut samples = 0;
            for sy in y * height / 36..(y + 1) * height / 36 {
                for sx in x * width / 36..(x + 1) * width / 36 {
                    alpha += pixels[(sy * width + sx) * 4 + 3] as u64;
                    samples += 1;
                }
            }
            rgba[(y * 36 + x) * 4 + 3] = ((alpha + samples / 2) / samples) as u8;
        }
    }
    rgba
}

fn icon(state: MenuBarState) -> tray_icon::Icon {
    let state = match state {
        MenuBarState::Idle | MenuBarState::Review(State::Reviewing) => {
            return tray_icon::Icon::from_rgba(gopher_rgba().to_vec(), 36, 36)
                .expect("valid gopher icon");
        }
        MenuBarState::Review(state) => state,
    };
    let mut rgba = vec![0_u8; 36 * 36 * 4];
    for y in 0..36 {
        for x in 0..36 {
            let px = x as f32;
            let py = y as f32;
            let ink = match state {
                State::Unknown => {
                    let hook = ((px - 18.).powi(2) + (py - 12.).powi(2)).sqrt();
                    (hook > 4. && hook < 7. && (py < 12. || px > 18.))
                        || distance(px, py, 23., 16., 18., 21.) < 1.8
                        || ((px - 18.).abs() < 1.8 && (25. ..29.).contains(&py))
                }
                State::Reviewing => unreachable!("reviewing uses the gopher artwork"),
                State::Comments => {
                    (px > 5. && px < 30. && py > 7. && py < 25.)
                        || (px > 8. && px < 14. && (25. ..30.).contains(&py))
                }
                State::Approved => {
                    distance(px, py, 6., 18., 14., 26.) < 2.2
                        || distance(px, py, 14., 26., 30., 9.) < 2.2
                }
            };
            if ink {
                rgba[(y * 36 + x) * 4 + 3] = 255;
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, 36, 36).expect("valid icon dimensions")
}
fn distance(x: f32, y: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let t = (((x - ax) * (bx - ax) + (y - ay) * (by - ay))
        / ((bx - ax).powi(2) + (by - ay).powi(2)))
    .clamp(0., 1.);
    ((x - ax - t * (bx - ax)).powi(2) + (y - ay - t * (by - ay)).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_icon_is_distinct_from_unknown_and_actionable_review_states() {
        let mut pr = worker::transition(Snapshot::default(), None, None, 100, 0);
        assert_eq!(menu_bar_state(&[], false), MenuBarState::Idle);
        assert_eq!(
            menu_bar_state(&[], true),
            MenuBarState::Review(State::Unknown)
        );
        assert_eq!(
            menu_bar_state(&[pr.clone()], false),
            MenuBarState::Review(State::Unknown)
        );
        for state in [State::Comments, State::Approved, State::Reviewing] {
            pr.state = state;
            pr.acknowledged = None;
            assert_eq!(
                menu_bar_state(&[pr.clone()], false),
                MenuBarState::Review(state)
            );
            pr.acknowledged = Some(pr.update_id.clone());
            assert_eq!(menu_bar_state(&[pr.clone()], false), MenuBarState::Idle);
        }
        pr.stale = true;
        pr.acknowledged = None;
        assert_eq!(
            menu_bar_state(&[pr], false),
            MenuBarState::Review(State::Unknown)
        );
    }

    #[test]
    fn bundled_gopher_decodes_to_a_transparent_template_icon() {
        let pixels = gopher_rgba();
        assert_eq!(pixels.len(), 36 * 36 * 4);
        assert_eq!(pixels[3], 0);
        assert!(
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| p[..3] == [0, 0, 0])
        );
        assert!(pixels.as_chunks::<4>().0.iter().any(|p| p[3] > 240));
        assert!(
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[3] > 0 && p[3] < 240)
        );
    }

    #[test]
    fn background_updates_wait_for_menu_close_and_coalesce() {
        let mut state = MenuState::default();
        state.request_update();
        assert!(state.can_update());
        state.installed(HashMap::new());
        state.begin_tracking();
        for _ in 0..5 {
            state.request_update();
            assert!(!state.can_update(), "An open menu must never be replaced");
        }
        state.tracking = false;
        assert!(
            state.can_update(),
            "Closing the menu must release the deferred update"
        );
        state.installed(HashMap::new());
        assert!(
            !state.can_update(),
            "Multiple refreshes need just one replacement"
        );
    }

    #[test]
    fn queued_close_event_cannot_replace_a_reopened_menu() {
        let mut state = MenuState::default();
        state.begin_tracking();
        state.request_update();
        state.tracking = false;
        state.begin_tracking();
        assert!(!state.can_update());
    }

    #[test]
    fn selection_keeps_displayed_update_after_close_and_refresh() {
        let mut state = MenuState::default();
        state.installed(HashMap::from([(
            "old-open".into(),
            Action::Open {
                url: "https://github.com/owner/repo/pull/1".into(),
                pr: "PR_1".into(),
                update: "displayed-update".into(),
            },
        )]));
        state.begin_tracking();
        state.request_update();
        state.tracking = false;
        // AppKit can queue the action after menu-close and worker-update events.
        for id in ["new-open", "newer-open"] {
            state.installed(HashMap::from([(
                id.into(),
                Action::Open {
                    url: "https://github.com/owner/repo/pull/1".into(),
                    pr: "PR_1".into(),
                    update: "new-update".into(),
                },
            )]));
        }
        assert!(
            matches!(state.action("old-open"), Some(Action::Open {update,..}) if update=="displayed-update")
        );
        state.begin_tracking();
        assert!(state.action("old-open").is_none());
    }

    #[test]
    fn notification_actions_reveal_only_for_body_clicks() {
        for (action, expected_reveal) in [
            ("com.apple.UNNotificationDefaultActionIdentifier", true),
            (ACKNOWLEDGE_ACTION, false),
        ] {
            assert!(
                matches!(notification_command(action, "notice".into()), Some(Command::NotificationAction {id,open:false,reveal}) if id=="notice" && reveal==expected_reveal)
            );
        }
        assert!(matches!(
            notification_command(OPEN_PR_ACTION, "notice".into()),
            Some(Command::NotificationAction {
                open: true,
                reveal: false,
                ..
            })
        ));
        for action in [
            "com.apple.UNNotificationDismissActionIdentifier",
            "unknown-action",
        ] {
            assert!(notification_command(action, "notice".into()).is_none());
        }
    }

    #[test]
    fn opening_acknowledges_only_after_browser_accepts_url() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let url = "https://github.com/owner/repo/pull/1";
        assert!(
            open_and_acknowledge(url, "PR_1", "update-1", &sender, |_| {
                anyhow::bail!("Browser failed")
            })
            .is_err()
        );
        assert!(receiver.try_recv().is_err());
        open_and_acknowledge(url, "PR_1", "update-1", &sender, |actual| {
            assert_eq!(actual, url);
            Ok(())
        })
        .unwrap();
        assert!(
            matches!(receiver.try_recv().unwrap(), Command::Acknowledge {pr,update,checked:true} if pr=="PR_1" && update=="update-1")
        );
    }
}
