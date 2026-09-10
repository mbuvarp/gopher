//! Local key routing and transactional registration of the single global shortcut.
use super::*;
use crate::hotkeys::{self, Binding, HotkeyAction, Preferences};
use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
    hotkey::{Code, HotKey, Modifiers},
};
use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags, NSTextView, NSWindow};
use objc2_foundation::NSRunLoop;
use std::{cell::RefCell, rc::Rc, str::FromStr};

#[derive(Clone, Debug)]
pub(super) enum Edit {
    Record(HotkeyAction),
    Clear(HotkeyAction),
    Reset,
}
#[derive(Clone, Default)]
pub(super) struct State {
    pub preferences: Preferences,
    pub recording: Option<HotkeyAction>,
    pub pending: bool,
    pub ready: bool,
    pub error: Option<String>,
    window: Option<Retained<NSWindow>>,
    inbox: bool,
}
trait Registry {
    fn register(&self, key: HotKey) -> Result<()>;
    fn unregister(&self, key: HotKey) -> Result<()>;
}
impl Registry for GlobalHotKeyManager {
    fn register(&self, key: HotKey) -> Result<()> {
        GlobalHotKeyManager::register(self, key).map_err(Into::into)
    }
    fn unregister(&self, key: HotKey) -> Result<()> {
        GlobalHotKeyManager::unregister(self, key).map_err(Into::into)
    }
}

struct Pending {
    preferences: Preferences,
    global: Option<HotKey>,
}
pub(super) struct Keyboard {
    pub state: Rc<RefCell<State>>,
    manager: Option<Box<dyn Registry>>,
    global: Option<HotKey>,
    pending: Option<Pending>,
    monitor: Option<Retained<AnyObject>>,
}
impl Keyboard {
    pub fn new(proxy: tao::event_loop::EventLoopProxy<AppEvent>) -> Self {
        let state = Rc::new(RefCell::new(State::default()));
        let input = state.clone();
        let local_proxy = proxy.clone();
        let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            // AppKit calls local monitors on the main thread, retaining the event.
            let key = unsafe { event.as_ref() };
            // Native menus own Escape and arrow navigation while tracking,
            // including header menus and the repository configuration dropdowns.
            if NSRunLoop::currentRunLoop().currentMode().as_deref()
                == Some(unsafe { objc2_app_kit::NSEventTrackingRunLoopMode })
            {
                return event.as_ptr();
            }
            let mut input = input.borrow_mut();
            let Some(window) = input
                .window
                .as_ref()
                .filter(|w| w.isVisible() && w.isKeyWindow())
            else {
                return event.as_ptr();
            };
            if key
                .window(objc2::MainThreadMarker::new().unwrap())
                .as_deref()
                != Some(&**window)
            {
                return event.as_ptr();
            }
            // Handle Escape before normal shortcut gating so AppKit cannot close
            // a subpanel (including one waiting for a settings write) itself.
            if key.keyCode() == 53 && modifiers(key.modifierFlags()) == 0 {
                if !key.isARepeat() {
                    let event = if input.recording.take().is_some() {
                        AppEvent::RefreshPopover
                    } else {
                        AppEvent::Escape
                    };
                    let _ = local_proxy.send_event(event);
                }
                return std::ptr::null_mut();
            }
            if !input.ready || input.pending {
                return event.as_ptr();
            }
            if let Some(action) = input.recording {
                if key.isARepeat() {
                    return std::ptr::null_mut();
                }
                input.recording = None;
                if key.keyCode() == 53 {
                    // Escape cancels recording without changing preferences.
                    let _ = local_proxy.send_event(AppEvent::RefreshPopover);
                } else {
                    let code = key.keyCode();
                    if let Some(name) = hotkeys::key_code_name(code) {
                        let label = match code {
                            49 => "Space".into(),
                            36 => "Return".into(),
                            123 => "←".into(),
                            124 => "→".into(),
                            125 => "↓".into(),
                            126 => "↑".into(),
                            96..=122 => name.into(),
                            _ => key
                                .charactersIgnoringModifiers()
                                .map(|s| s.to_string().to_uppercase())
                                .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
                                .unwrap_or_else(|| name.into()),
                        };
                        let binding = Binding {
                            key: code,
                            modifiers: modifiers(key.modifierFlags()),
                            label,
                        };
                        let _ = local_proxy.send_event(AppEvent::ShortcutRecorded(action, binding));
                    } else {
                        input.error = Some("Choose a letter, number, punctuation, arrow, or function key. Escape cancels.".into());
                        let _ = local_proxy.send_event(AppEvent::RefreshPopover);
                    }
                }
                return std::ptr::null_mut();
            }
            if hotkeys::is_settings_shortcut(key.keyCode(), modifiers(key.modifierFlags())) {
                if !key.isARepeat() {
                    let _ = local_proxy.send_event(AppEvent::PopoverAction(Action::Config));
                }
                return std::ptr::null_mut();
            }
            if !input.inbox
                || window
                    .firstResponder()
                    .is_some_and(|r| r.downcast_ref::<NSTextView>().is_some())
            {
                return event.as_ptr();
            }
            if let Some(action) = input
                .preferences
                .action(key.keyCode(), modifiers(key.modifierFlags()))
            {
                if action == HotkeyAction::OpenGopher {
                    return event.as_ptr();
                }
                if !key.isARepeat() || matches!(action, HotkeyAction::Next | HotkeyAction::Previous)
                {
                    let _ = local_proxy.send_event(AppEvent::Shortcut(action));
                }
                return std::ptr::null_mut();
            }
            event.as_ptr()
        });
        let monitor = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &handler)
        };
        GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
            if event.state == HotKeyState::Pressed {
                let _ = proxy.send_event(AppEvent::GlobalShortcut(event.id));
            }
        }));
        let manager = GlobalHotKeyManager::new()
            .map(|manager| Box::new(manager) as Box<dyn Registry>)
            .map_err(|error| {
                state.borrow_mut().error = Some(format!("Global shortcut unavailable: {error}"));
            })
            .ok();
        if monitor.is_none() {
            state.borrow_mut().error = Some("Could not install keyboard shortcuts".into());
        }
        Self {
            state,
            manager,
            global: None,
            pending: None,
            monitor,
        }
    }
    pub fn sync(&self, window: Option<Retained<NSWindow>>, inbox: bool, settings: bool) {
        let mut state = self.state.borrow_mut();
        if !settings || window.as_ref().is_none_or(|w| !w.isVisible()) {
            state.recording = None;
        }
        state.window = window;
        state.inbox = inbox;
    }
    pub fn load(&mut self, preferences: Preferences, error: Option<String>) {
        let mut state = self.state.borrow_mut();
        state.preferences = preferences;
        state.ready = true;
        if error.is_some() {
            state.error = error;
        }
        match global_key(&state.preferences).and_then(|key| {
            if let Some(key) = key {
                self.manager
                    .as_ref()
                    .context("Global shortcuts are unavailable")?
                    .register(key)?;
            }
            Ok(key)
        }) {
            Ok(key) => self.global = key,
            Err(error) => state.error = Some(format!("Cannot register Open Gopher: {error}")),
        }
    }
    pub fn global_event(&mut self, id: u32, sender: &UnboundedSender<Command>) -> bool {
        if !self.global.is_some_and(|key| key.id == id) {
            return false;
        }
        let recording = self.state.borrow().recording;
        if let Some(action) = recording {
            if self
                .state
                .borrow()
                .window
                .as_ref()
                .is_some_and(|w| w.isKeyWindow() && w.isVisible())
            {
                let binding = self
                    .state
                    .borrow()
                    .preferences
                    .binding(HotkeyAction::OpenGopher);
                self.recorded(action, binding, sender);
            }
            false
        } else {
            true
        }
    }
    pub fn edit(&mut self, edit: Edit, sender: &UnboundedSender<Command>) {
        if !self.state.borrow().ready || self.state.borrow().pending {
            return;
        }
        self.state.borrow_mut().error = None;
        match edit {
            Edit::Record(action) => self.state.borrow_mut().recording = Some(action),
            Edit::Clear(action) => self.recorded(action, None, sender),
            Edit::Reset => self.save(Ok(Preferences::default()), sender),
        }
    }
    pub fn recorded(
        &mut self,
        action: HotkeyAction,
        binding: Option<Binding>,
        sender: &UnboundedSender<Command>,
    ) {
        let next = self.state.borrow().preferences.changed(action, binding);
        self.save(next, sender);
    }
    fn save(&mut self, next: Result<Preferences>, sender: &UnboundedSender<Command>) {
        if !self.state.borrow().ready || self.pending.is_some() {
            return;
        }
        self.state.borrow_mut().recording = None;
        let result = (|| -> Result<()> {
            let preferences = next?;
            preferences.validate()?;
            let global = global_key(&preferences)?;
            // Keep the previous registration usable until SQLite confirms the write.
            if global != self.global
                && let Some(key) = global
            {
                self.manager
                    .as_ref()
                    .context("Global shortcuts are unavailable")?
                    .register(key)
                    .context("Shortcut is unavailable or already registered by another app")?;
            }
            self.pending = Some(Pending {
                preferences: preferences.clone(),
                global,
            });
            self.state.borrow_mut().pending = true;
            if sender.send(Command::SaveHotkeys(preferences)).is_err() {
                self.saved(Err("Settings worker is unavailable".into()));
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.state.borrow_mut().error = Some(error.to_string());
        }
    }
    pub fn saved(&mut self, result: std::result::Result<(), String>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let mut state = self.state.borrow_mut();
        state.pending = false;
        let obsolete = if result.is_ok() {
            self.global
        } else {
            pending.global
        };
        if pending.global != self.global
            && let (Some(manager), Some(key)) = (&self.manager, obsolete)
            && let Err(error) = manager.unregister(key)
        {
            tracing::error!(event="hotkey_unregister_failed",error=%error);
        }
        match result {
            Ok(()) => {
                self.global = pending.global;
                state.preferences = pending.preferences;
                state.error = None;
            }
            Err(error) => state.error = Some(format!("Could not save shortcuts: {error}")),
        }
    }
}
impl Drop for Keyboard {
    fn drop(&mut self) {
        if let Some(monitor) = &self.monitor {
            unsafe {
                NSEvent::removeMonitor(monitor);
            }
        }
        if let Some(manager) = &self.manager {
            if let Some(key) = self.global {
                let _ = manager.unregister(key);
            }
            if let Some(pending) = &self.pending
                && pending.global != self.global
                && let Some(key) = pending.global
            {
                let _ = manager.unregister(key);
            }
        }
    }
}
fn modifiers(flags: NSEventModifierFlags) -> u8 {
    let mut value = 0;
    for (flag, mask) in [
        (NSEventModifierFlags::Command, hotkeys::COMMAND),
        (NSEventModifierFlags::Option, hotkeys::OPTION),
        (NSEventModifierFlags::Control, hotkeys::CONTROL),
        (NSEventModifierFlags::Shift, hotkeys::SHIFT),
    ] {
        if flags.contains(flag) {
            value |= mask;
        }
    }
    value
}
fn global_key(preferences: &Preferences) -> Result<Option<HotKey>> {
    let Some(binding) = preferences.binding(HotkeyAction::OpenGopher) else {
        return Ok(None);
    };
    let mut mods = Modifiers::empty();
    for (flag, mask) in [
        (Modifiers::SUPER, hotkeys::COMMAND),
        (Modifiers::ALT, hotkeys::OPTION),
        (Modifiers::CONTROL, hotkeys::CONTROL),
        (Modifiers::SHIFT, hotkeys::SHIFT),
    ] {
        if binding.modifiers & mask != 0 {
            mods |= flag;
        }
    }
    let code =
        Code::from_str(hotkeys::key_code_name(binding.key).context("Unsupported shortcut key")?)?;
    Ok(Some(HotKey::new(Some(mods), code)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Registrations {
        keys: Vec<HotKey>,
        reject: bool,
    }
    struct MockRegistry(Rc<RefCell<Registrations>>);
    impl Registry for MockRegistry {
        fn register(&self, key: HotKey) -> Result<()> {
            let mut state = self.0.borrow_mut();
            anyhow::ensure!(!state.reject, "Shortcut occupied");
            state.keys.push(key);
            Ok(())
        }
        fn unregister(&self, key: HotKey) -> Result<()> {
            self.0.borrow_mut().keys.retain(|k| *k != key);
            Ok(())
        }
    }
    fn preferences(key: u16) -> Preferences {
        Preferences::default()
            .changed(
                HotkeyAction::OpenGopher,
                Some(Binding {
                    key,
                    modifiers: hotkeys::COMMAND,
                    label: "Test".into(),
                }),
            )
            .unwrap()
    }
    #[test]
    fn global_shortcut_changes_wait_for_persistence_and_roll_back_on_failure() {
        let registrations = Rc::new(RefCell::new(Registrations::default()));
        let mut keyboard = Keyboard {
            state: Rc::new(RefCell::new(State::default())),
            manager: Some(Box::new(MockRegistry(registrations.clone()))),
            global: None,
            pending: None,
            monitor: None,
        };
        let old = preferences(5);
        keyboard.load(old.clone(), None);
        let old_key = keyboard.global.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let next = preferences(4);
        keyboard.save(Ok(next.clone()), &tx);
        assert!(matches!(rx.try_recv().unwrap(),Command::SaveHotkeys(p) if p==next));
        assert_eq!(registrations.borrow().keys.len(), 2);
        assert_eq!(keyboard.global, Some(old_key));
        assert_eq!(keyboard.state.borrow().preferences, old);
        keyboard.saved(Err("disk unavailable".into()));
        assert_eq!(registrations.borrow().keys, vec![old_key]);
        assert_eq!(keyboard.state.borrow().preferences, old);
        keyboard.save(Ok(next.clone()), &tx);
        keyboard.saved(Ok(()));
        assert_eq!(registrations.borrow().keys, vec![keyboard.global.unwrap()]);
        assert_eq!(keyboard.state.borrow().preferences, next);
        keyboard.save(Ok(Preferences::default()), &tx);
        keyboard.saved(Ok(()));
        assert!(registrations.borrow().keys.is_empty());
        assert!(keyboard.global.is_none());
    }
    #[test]
    fn occupied_global_shortcut_does_not_save_or_replace_the_previous_binding() {
        let registrations = Rc::new(RefCell::new(Registrations::default()));
        let mut keyboard = Keyboard {
            state: Rc::new(RefCell::new(State::default())),
            manager: Some(Box::new(MockRegistry(registrations.clone()))),
            global: None,
            pending: None,
            monitor: None,
        };
        let old = preferences(5);
        keyboard.load(old.clone(), None);
        let old_key = keyboard.global.unwrap();
        registrations.borrow_mut().reject = true;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        keyboard.save(Ok(preferences(4)), &tx);
        assert!(rx.try_recv().is_err());
        assert_eq!(keyboard.global, Some(old_key));
        assert_eq!(keyboard.state.borrow().preferences, old);
        assert!(keyboard.state.borrow().error.is_some());
        assert!(keyboard.pending.is_none());
    }
}
