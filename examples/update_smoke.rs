//! Opt-in, isolated Sparkle integration host. See docs/updates.md. Never polls GitHub.
use objc2::{MainThreadMarker, Message, msg_send};
use objc2_app_kit::{NSApplication, NSButton, NSView};
use objc2_foundation::{NSBundle, NSString};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tao::{
    event::Event,
    event_loop::{ControlFlow, EventLoopBuilder},
    platform::{
        macos::{ActivationPolicy, EventLoopExtMacOS},
        run_return::EventLoopExtRunReturn,
    },
};
#[derive(Clone, Debug)]
enum AppEvent {
    UpdaterChanged,
    Shutdown(&'static str),
    Drained,
}
#[allow(dead_code)]
#[path = "../src/macos/updates.rs"]
mod updates;

fn property(key: &str) -> String {
    let value = NSBundle::mainBundle()
        .objectForInfoDictionaryKey(&NSString::from_str(key))
        .expect("Missing smoke-test bundle metadata");
    let value: objc2::rc::Retained<NSString> = unsafe { msg_send![&value, description] };
    value.to_string()
}
fn find_button(view: &NSView, title: &str) -> Option<objc2::rc::Retained<NSButton>> {
    for child in view.subviews() {
        if let Some(button) = child.downcast_ref::<NSButton>()
            && button.title().to_string() == title
        {
            return Some(button.retain());
        }
        if let Some(button) = find_button(&child, title) {
            return Some(button);
        }
    }
    None
}
fn main() {
    assert!(property("CFBundleIdentifier").starts_with("dev.mbuvarp.gopher.update-smoke."));
    let directory = PathBuf::from(property("GopherSmokeDirectory"));
    assert!(directory.is_absolute());
    std::fs::create_dir_all(&directory).unwrap();
    let expect_no_update = NSBundle::mainBundle()
        .objectForInfoDictionaryKey(&NSString::from_str("GopherSmokeExpectNoUpdate"))
        .is_some_and(|v| unsafe { msg_send![&v, boolValue] });
    let mode = NSBundle::mainBundle()
        .objectForInfoDictionaryKey(&NSString::from_str("GopherSmokeMode"))
        .map(|v| {
            let value: objc2::rc::Retained<NSString> = unsafe { msg_send![&v, description] };
            value.to_string()
        })
        .unwrap_or_default();
    if !expect_no_update && property("CFBundleVersion") == "1.1.1" {
        assert!(directory.join("mutation-persisted").exists());
        assert!(directory.join("clean-shutdown").exists());
        std::fs::write(directory.join("relaunched"), "1.1.1").unwrap();
        return;
    }
    let mut event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    let proxy = event_loop.create_proxy();
    let updater = updates::Updater::new(proxy.clone());
    let started = Instant::now();
    let mut opened = false;
    let mut clicked = false;
    let mut quitting = false;
    let mut reminder_seen = None;
    let mut dismissed = false;
    event_loop.run_return(|event, _, flow| {
        *flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(100));
        match event {
            Event::UserEvent(AppEvent::Shutdown(reason)) => {
                assert!(clicked, "Quit occurred before the requested update action");
                if !quitting {
                    quitting = true;
                    std::fs::write(directory.join("quit-request"), reason).unwrap();
                    let proxy = proxy.clone();
                    let directory = directory.clone();
                    std::thread::spawn(move || {
                        // Model a submitted mutation finishing and being persisted.
                        std::thread::sleep(Duration::from_secs(3));
                        std::fs::write(directory.join("mutation-persisted"), "done").unwrap();
                        proxy.send_event(AppEvent::Drained).unwrap();
                    });
                }
            }
            Event::UserEvent(AppEvent::Drained) => {
                *flow = ControlFlow::Exit;
            }
            Event::MainEventsCleared if !quitting => {
                let state = updater.state();
                std::fs::write(directory.join("state"), format!("{state:?}")).unwrap();
                if mode == "reminder" {
                    if state.available && !state.downloads && reminder_seen.is_none() {
                        reminder_seen = Some(Instant::now());
                        // Exercise the actual SDK callbacks for dialog dismissal:
                        // an undownloaded update must remain advertised afterward.
                        updater.check();
                    }
                    if reminder_seen.is_some() && !dismissed {
                        let app =
                            NSApplication::sharedApplication(MainThreadMarker::new().unwrap());
                        for window in app.windows() {
                            if let Some(view) = window.contentView()
                                && let Some(button) = find_button(&view, "Remind Me Later")
                            {
                                dismissed = true;
                                reminder_seen = Some(Instant::now());
                                unsafe {
                                    button.performClick(None);
                                }
                            }
                        }
                    }
                    if dismissed
                        && reminder_seen.is_some_and(|seen| seen.elapsed() > Duration::from_secs(3))
                    {
                        assert!(state.available, "Undownloaded update reminder disappeared");
                        std::fs::write(directory.join("reminder-retained"), "done").unwrap();
                        *flow = ControlFlow::Exit;
                        return;
                    }
                }
                if expect_no_update
                    && (state.message.starts_with("Could not complete")
                        || state.message == "Gopher is up to date.")
                {
                    std::fs::write(directory.join("rejected"), &state.message).unwrap();
                    *flow = ControlFlow::Exit;
                    return;
                }
                if !expect_no_update
                    && state.available
                    && state.can_check
                    && state.message.starts_with("Update downloaded.")
                    && !opened
                {
                    opened = true;
                    if mode == "menu" {
                        // Match Action::Quit: drain, leave Tao, then return from
                        // main without first requesting NSApplication termination.
                        clicked = true;
                        proxy.send_event(AppEvent::Shutdown("quit")).unwrap();
                    } else {
                        updater.check();
                    }
                }
                if opened && !clicked {
                    let app = NSApplication::sharedApplication(MainThreadMarker::new().unwrap());
                    for window in app.windows() {
                        if let Some(view) = window.contentView()
                            && let Some(button) = find_button(&view, "Install and Relaunch")
                        {
                            clicked = true;
                            unsafe {
                                button.performClick(None);
                            }
                        }
                    }
                }
                if started.elapsed() > Duration::from_secs(90) {
                    std::fs::write(directory.join("failed"), "Update did not finish").unwrap();
                    *flow = ControlFlow::Exit;
                }
            }
            _ => {}
        }
    });
    drop(updater);
    drop(event_loop);
    std::fs::write(directory.join("clean-shutdown"), "done").unwrap();
    updates::finish_native_termination();
}
