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
    runtime::{Bool, ProtocolObject},
};
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSBundle, NSError, NSObject, NSObjectProtocol, NSString, NSURL};
use objc2_service_management::{SMAppService, SMAppServiceStatus};
use objc2_user_notifications::*;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
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
    TrayIcon, TrayIconBuilder,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
};

#[derive(Clone, Debug)]
enum AppEvent {
    Worker(UiEvent),
    Menu(String),
    Permission(bool),
    NotificationError(String),
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
            if action == "com.apple.UNNotificationDefaultActionIdentifier" {
                let id=response.notification().request().identifier().to_string();
                let _=self.ivars().send(Command::NotificationClicked(id));
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

enum Action {
    Open(String),
    Ack {
        pr: String,
        update: String,
        checked: bool,
    },
    Refresh,
    Config,
    Logs,
    Login,
    NotificationSettings,
    Quit,
}

pub fn run(directory: PathBuf, config: Config) -> Result<()> {
    let mut event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    event_loop.set_dock_visibility(false);
    event_loop.set_activate_ignoring_other_apps(false);
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
    let mut tray: Option<TrayIcon> = None;
    let mut actions = HashMap::new();
    let mut prs = Vec::new();
    let mut service_error = None;
    let mut ui_error = if bundled {
        None
    } else {
        Some("Notifications require the packaged Gopher.app; run scripts/bundle.sh.".to_owned())
    };
    let mut pending = Vec::new();
    let proxy = event_loop.create_proxy();
    event_loop.run_return(|event,_,flow| {
        *flow=ControlFlow::Wait;
        let mut rebuild=false;
        match event {
            Event::NewEvents(StartCause::Init) => {
                match TrayIconBuilder::new().with_tooltip("Gopher — GitHub reviews").with_icon(icon(State::Unknown)).with_icon_as_template(true).build() {
                    Ok(icon)=>{tray=Some(icon);tracing::info!(event="menu_bar_created");},
                    Err(e)=>{eprintln!("Cannot create menu bar icon: {e}");*flow=ControlFlow::Exit;}
                }
                rebuild=true;
            }
            Event::UserEvent(AppEvent::Permission(granted)) => {
                permission=Some(granted);
                if !granted {ui_error=Some("Notifications are disabled. Enable Gopher in System Settings → Notifications.".into());}
                else if ui_error.as_deref().is_some_and(|e|e.starts_with("Notifications are disabled")){ui_error=None;}
                tracing::info!(event="notification_permission",granted);
                if granted { for (id,title,body) in pending.drain(..) {
                    if let Some(center)=&center {notify(center,id,title,body,sender.clone(),proxy.clone());}
                }} else {for (id,_,_) in pending.drain(..){let _=sender.send(Command::NotificationFailed(id));}}
                rebuild=true;
            }
            Event::UserEvent(AppEvent::NotificationError(message)) => {ui_error=Some(message);rebuild=true;}
            Event::UserEvent(AppEvent::Worker(event)) => match event {
                UiEvent::Updated{prs:updated,error}=>{prs=updated;service_error=error;rebuild=true;}
                UiEvent::Open{url,pr,update}=>{match open_url(&url){
                    Ok(())=>{let _=sender.send(Command::Acknowledge{pr,update,checked:true});}
                    Err(e)=>{ui_error=Some(e.to_string());rebuild=true;}
                }}
                UiEvent::Notify{id,title,body}=>{
                    if permission==Some(true) {
                        if let Some(center)=&center {notify(center,id,title,body,sender.clone(),proxy.clone());}
                    } else if permission.is_none() && bundled {pending.push((id,title,body));}
                    else {let _=sender.send(Command::NotificationFailed(id));}
                }
            },
            Event::UserEvent(AppEvent::Menu(id)) => {
                if let Some(action)=actions.get(&id) {
                    let result: Result<()> = (|| {
                        match action {
                            Action::Open(url)=>open_url(url)?,
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
                            Action::Quit=>{let _=sender.send(Command::Shutdown);*flow=ControlFlow::Exit;}
                        }
                        Ok(())
                    })();
                    if let Err(e)=result {tracing::error!(event="menu_action_failed",error=%e);ui_error=Some(e.to_string());rebuild=true;}
                }
            }
            _=>(),
        }
        if rebuild
            && let Some(tray)=&tray {
                let error=service_error.as_deref().or(ui_error.as_deref());
                match menu(&prs,error,bundled) {
                    Ok((menu,new_actions))=>{tray.set_menu(Some(Box::new(menu)));actions=new_actions;}
                    Err(e)=>tracing::error!(event="menu_build_failed",error=%e),
                }
                let attention:Vec<_>=prs.iter().filter(|p|p.needs_attention()).collect();
                let state=if error.is_some(){State::Unknown}else if attention.iter().any(|p|p.state==State::Comments){State::Comments}else if !attention.is_empty(){State::Approved}else if prs.iter().any(|p|!p.stale && p.state==State::Reviewing && p.acknowledged.as_deref()!=Some(&p.update_id)){State::Reviewing}else{State::Unknown};
                let _=tray.set_icon_with_as_template(Some(icon(state)),true);
                tray.set_title(if error.is_some(){Some("!")}else{None});
                let _=tray.set_tooltip(Some(format!("Gopher · {} PRs · {} updates",prs.len(),attention.len())));
                tracing::debug!(event="menu_updated",prs=prs.len(),updates=attention.len(),state=?state);
            }
    });
    let _ = sender.send(Command::Shutdown);
    drop(delegate);
    Ok(())
}

fn notify(
    center: &UNUserNotificationCenter,
    id: String,
    title: String,
    body: String,
    sender: UnboundedSender<Command>,
    proxy: tao::event_loop::EventLoopProxy<AppEvent>,
) {
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(&title));
    content.setBody(&NSString::from_str(&body));
    content.setSound(Some(&UNNotificationSound::defaultSound()));
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

fn menu(
    prs: &[PullRequest],
    error: Option<&str>,
    bundled: bool,
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
    sorted.sort_by_key(|p| (&p.snapshot.repo, std::cmp::Reverse(p.snapshot.number)));
    let mut last_repo = "";
    for pr in sorted {
        if pr.snapshot.repo != last_repo {
            menu.append(&PredefinedMenuItem::separator())?;
            menu.append(&MenuItem::new(&pr.snapshot.repo, false, None))?;
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
                "{} #{} {}{}",
                state.symbol(),
                pr.snapshot.number,
                title,
                if pr.needs_attention() { " •" } else { "" }
            ),
            true,
        );
        let open = MenuItem::new("Open PR", true, None);
        actions.insert(open.id().0.clone(), Action::Open(pr.snapshot.url.clone()));
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
        item.append(&PredefinedMenuItem::separator())?;
        item.append(&MenuItem::new(
            format!(
                "{}{} · {} unresolved threads",
                state.label(),
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
        menu.append(&item)?;
    }
    menu.append(&PredefinedMenuItem::separator())?;
    for (label, action) in [
        ("Refresh now", Action::Refresh),
        ("Edit configuration… (restart to apply)", Action::Config),
        ("Open logs…", Action::Logs),
        ("Notification settings…", Action::NotificationSettings),
    ] {
        let item = MenuItem::new(label, true, None);
        actions.insert(item.id().0.clone(), action);
        menu.append(&item)?;
    }
    let enabled = bundled
        && unsafe { SMAppService::mainAppService().status() == SMAppServiceStatus::Enabled };
    let login = CheckMenuItem::new("Launch at login", bundled, enabled, None);
    actions.insert(login.id().0.clone(), Action::Login);
    menu.append(&login)?;
    let quit = MenuItem::new("Quit Gopher", true, None);
    actions.insert(quit.id().0.clone(), Action::Quit);
    menu.append(&quit)?;
    Ok((menu, actions))
}

fn icon(state: State) -> tray_icon::Icon {
    let mut rgba = vec![0_u8; 36 * 36 * 4];
    for y in 0..36 {
        for x in 0..36 {
            let px = x as f32;
            let py = y as f32;
            let radius = ((px - 18.).powi(2) + (py - 18.).powi(2)).sqrt();
            let ink = match state {
                State::Unknown => {
                    let hook = ((px - 18.).powi(2) + (py - 12.).powi(2)).sqrt();
                    (hook > 4. && hook < 7. && (py < 12. || px > 18.))
                        || distance(px, py, 23., 16., 18., 21.) < 1.8
                        || ((px - 18.).abs() < 1.8 && (25. ..29.).contains(&py))
                }
                State::Reviewing => radius > 11. && radius < 14. && !(px > 20. && py < 15.),
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
