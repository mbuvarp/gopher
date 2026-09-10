//! Main-thread Sparkle adapter. Load only the framework embedded in a release
//! bundle; CLI/development builds never discover or execute an external updater.
use super::AppEvent;
use anyhow::{Context, Result};
use objc2::{
    DefinedClass, MainThreadOnly, define_class, msg_send,
    rc::{Allocated, Retained},
    runtime::{AnyClass, AnyObject, ProtocolObject, Sel},
};
use objc2_app_kit::{NSApplication, NSApplicationDelegate, NSApplicationTerminateReply};
use objc2_foundation::{
    NSBundle, NSError, NSKeyValueObservingOptions, NSObject, NSObjectProtocol, NSString,
};
use std::cell::{Cell, RefCell};
use tao::event_loop::EventLoopProxy;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct State {
    pub enabled: bool,
    pub available: bool,
    pub can_check: bool,
    pub checks: bool,
    pub downloads: bool,
    pub message: String,
}
impl State {
    pub fn menu_title(&self) -> &'static str {
        if self.available {
            "Update available…"
        } else {
            "Check for updates…"
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub(super) enum Edit {
    Checks,
    Downloads,
}

struct DelegateState {
    proxy: EventLoopProxy<AppEvent>,
    original: Option<Retained<ProtocolObject<dyn NSApplicationDelegate>>>,
    available: Cell<bool>,
    on_quit: Cell<bool>,
    terminating: Cell<bool>,
    message: RefCell<String>,
}
impl DelegateState {
    fn changed(&self) {
        let _ = self.proxy.send_event(AppEvent::UpdaterChanged);
    }
}
thread_local! {
    static NATIVE_TERMINATION: Cell<bool> = const { Cell::new(false) };
    // NSApplication's delegate is weak. Keep Tao's delegate alive after its
    // event loop is dropped, until the final native termination callbacks.
    static TERMINATION_DELEGATE: RefCell<Option<Retained<ProtocolObject<dyn NSApplicationDelegate>>>> = const { RefCell::new(None) };
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "GopherUpdateDelegate"]
    #[ivars = DelegateState]
    struct Delegate;
    unsafe impl NSObjectProtocol for Delegate {
        #[unsafe(method(respondsToSelector:))]
        fn responds(&self, selector: Sel) -> bool {
            let own: bool = unsafe { msg_send![super(self), respondsToSelector: selector] };
            own || self.ivars().original.as_ref().is_some_and(|d| d.respondsToSelector(selector))
        }
    }
    unsafe impl NSApplicationDelegate for Delegate {
        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _: &NSApplication) -> NSApplicationTerminateReply {
            NATIVE_TERMINATION.set(true);
            if !self.ivars().terminating.replace(true) {
                let _ = self.ivars().proxy.send_event(AppEvent::Shutdown("native_termination"));
            }
            // NSTerminateLater enters AppKit's modal termination loop, which
            // prevents Tao run_return from unwinding. Cancel this request and
            // issue native termination again after Rust finishes its cleanup.
            NSApplicationTerminateReply::TerminateCancel
        }
    }
    impl Delegate {
        #[unsafe(method_id(forwardingTargetForSelector:))]
        fn forward(&self, _: Sel) -> Option<Retained<AnyObject>> {
            self.ivars().original.as_ref().map(|d| unsafe { Retained::cast_unchecked(d.clone()) })
        }
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe(&self, _: &NSString, _: &AnyObject, _: &AnyObject, _: *mut std::ffi::c_void) {
            self.ivars().changed();
        }
        #[unsafe(method(supportsGentleScheduledUpdateReminders))]
        fn gentle(&self) -> bool { true }
        #[unsafe(method(standardUserDriverShouldHandleShowingScheduledUpdate:andInImmediateFocus:))]
        fn scheduled(&self, _: &AnyObject, _: bool) -> bool { false }
        #[unsafe(method(standardUserDriverWillHandleShowingUpdate:forUpdate:state:))]
        fn showing(&self, _: bool, _: &AnyObject, _: &AnyObject) {
            self.ivars().available.set(true);
            self.ivars().changed();
        }
        #[unsafe(method(updater:userDidMakeChoice:forUpdate:state:))]
        fn choice(&self, _: &AnyObject, choice: isize, _: &AnyObject, _: &AnyObject) {
            if choice == 0 { // SPUUserUpdateChoiceSkip
                self.ivars().on_quit.set(false);
                self.ivars().available.set(false);
                self.ivars().message.borrow_mut().clear();
                self.ivars().changed();
            }
        }
        #[unsafe(method(standardUserDriverWillFinishUpdateSession))]
        fn finished_session(&self) {
            // Finishing/dismissing the dialog does not withdraw the update.
            // Skip and failed/no-update checks clear it in their own callbacks.
            self.ivars().changed();
        }
        #[unsafe(method(updater:didFindValidUpdate:))]
        fn found(&self, _: &AnyObject, _: &AnyObject) {
            self.ivars().available.set(true);
            *self.ivars().message.borrow_mut() = "An update is available.".into();
            tracing::info!(event="update_available");
            self.ivars().changed();
        }
        #[unsafe(method(updater:willInstallUpdateOnQuit:immediateInstallationBlock:))]
        fn on_quit(&self, _: &AnyObject, _: &AnyObject, _: &block2::DynBlock<dyn Fn()>) -> bool {
            self.ivars().on_quit.set(true);
            self.ivars().available.set(true);
            *self.ivars().message.borrow_mut() = "Update downloaded. It will install when Gopher quits.".into();
            tracing::info!(event="update_ready_on_quit");
            self.ivars().changed();
            false // Leave scheduling to Sparkle; never invoke an unsolicited restart.
        }
        #[unsafe(method(updater:didFinishUpdateCycleForUpdateCheck:error:))]
        fn finished(&self, _: &AnyObject, _: isize, error: Option<&NSError>) {
            if let Some(error) = error {
                // Codes/domains suffice for diagnosis without arbitrary server text.
                if error.domain().to_string() == "SUSparkleErrorDomain" && error.code() == 1001 {
                    tracing::info!(event="update_check_current"); // SUNoUpdateError
                    *self.ivars().message.borrow_mut() = "Gopher is up to date.".into();
                } else if error.domain().to_string() == "SUSparkleErrorDomain" && error.code() == 4007 {
                    tracing::info!(event="update_cancelled");
                    self.ivars().message.borrow_mut().clear();
                } else {
                    tracing::warn!(event="update_cycle_failed", domain=%error.domain(), code=error.code());
                    *self.ivars().message.borrow_mut() = "Could not complete the update. Try Check for updates again.".into();
                }
                self.ivars().available.set(false);
                self.ivars().on_quit.set(false);
            }
            self.ivars().changed();
        }
    }
);

const OBSERVED: &[&str] = &[
    "canCheckForUpdates",
    "automaticallyChecksForUpdates",
    "automaticallyDownloadsUpdates",
];
pub(super) struct Updater {
    delegate: Retained<Delegate>,
    controller: Option<Retained<AnyObject>>,
    updater: Option<Retained<AnyObject>>,
    _framework: Option<Retained<NSBundle>>,
    disabled_reason: String,
}
impl Updater {
    pub fn new(proxy: EventLoopProxy<AppEvent>) -> Self {
        let mtm = objc2::MainThreadMarker::new().unwrap();
        let app = NSApplication::sharedApplication(mtm);
        let allocated = Delegate::alloc(mtm).set_ivars(DelegateState {
            proxy,
            original: app.delegate(),
            available: Cell::new(false),
            on_quit: Cell::new(false),
            terminating: Cell::new(false),
            message: RefCell::new(String::new()),
        });
        let delegate: Retained<Delegate> = unsafe { msg_send![super(allocated), init] };
        app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        let mut this = Self {
            delegate,
            controller: None,
            updater: None,
            _framework: None,
            disabled_reason: "Updates are disabled in development builds.".into(),
        };
        let bundle = NSBundle::mainBundle();
        let enabled = bundle
            .objectForInfoDictionaryKey(&NSString::from_str("GopherUpdatesEnabled"))
            .is_some_and(|v| unsafe { msg_send![&v, boolValue] });
        if enabled && let Err(error) = this.start(&bundle) {
            tracing::error!(event="updater_start_failed", error=%error);
            this.disabled_reason =
                "Updates could not start. Reinstall Gopher using the installer.".into();
        }
        this
    }
    fn start(&mut self, bundle: &NSBundle) -> Result<()> {
        let framework = NSBundle::bundleWithPath(&NSString::from_str(&format!(
            "{}/Contents/Frameworks/Sparkle.framework",
            bundle.bundlePath()
        )))
        .context("Missing bundled Sparkle framework")?;
        unsafe { framework.loadAndReturnError() }
            .map_err(|e| anyhow::anyhow!("Cannot load Sparkle: {}", e.code()))?;
        let class =
            AnyClass::get(c"SPUStandardUpdaterController").context("Missing Sparkle controller")?;
        let controller: Allocated<AnyObject> = unsafe { msg_send![class, alloc] };
        let controller: Retained<AnyObject> = unsafe {
            msg_send![controller, initWithStartingUpdater: false, updaterDelegate: &*self.delegate, userDriverDelegate: &*self.delegate]
        };
        let updater: Retained<AnyObject> = unsafe { msg_send![&controller, updater] };
        let mut error: Option<Retained<NSError>> = None;
        let ok: bool = unsafe { msg_send![&updater, startUpdater: &mut error] };
        anyhow::ensure!(
            ok,
            "Sparkle rejected its configuration (code {})",
            error.as_ref().map_or(0, |e| e.code())
        );
        for key in OBSERVED {
            unsafe {
                let _: () = msg_send![&updater, addObserver: &*self.delegate, forKeyPath: &*NSString::from_str(key), options: NSKeyValueObservingOptions::empty(), context: std::ptr::null_mut::<std::ffi::c_void>()];
            }
        }
        self.controller = Some(controller);
        self.updater = Some(updater);
        self._framework = Some(framework);
        tracing::info!(event = "updater_started");
        Ok(())
    }
    pub fn state(&self) -> State {
        let Some(updater) = &self.updater else {
            return State {
                message: self.disabled_reason.clone(),
                ..Default::default()
            };
        };
        State {
            enabled: true,
            available: self.delegate.ivars().available.get(),
            can_check: unsafe { msg_send![updater, canCheckForUpdates] },
            checks: unsafe { msg_send![updater, automaticallyChecksForUpdates] },
            downloads: unsafe { msg_send![updater, automaticallyDownloadsUpdates] },
            message: self.delegate.ivars().message.borrow().clone(),
        }
    }
    pub fn check(&self) {
        if self.state().can_check
            && let Some(controller) = &self.controller
        {
            tracing::info!(event = "update_check_requested");
            unsafe {
                let _: () = msg_send![controller, checkForUpdates: std::ptr::null::<AnyObject>()];
            }
        }
    }
    pub fn edit(&self, edit: Edit) {
        if let Some(updater) = &self.updater {
            let state = self.state();
            unsafe {
                match edit {
                    Edit::Checks => {
                        let _: () =
                            msg_send![updater, setAutomaticallyChecksForUpdates: !state.checks];
                    }
                    Edit::Downloads => {
                        let _: () =
                            msg_send![updater, setAutomaticallyDownloadsUpdates: !state.downloads];
                    }
                }
            }
            self.delegate.ivars().changed();
        }
    }
}
impl Drop for Updater {
    fn drop(&mut self) {
        if let Some(updater) = &self.updater {
            for key in OBSERVED {
                unsafe {
                    let _: () = msg_send![updater, removeObserver: &*self.delegate, forKeyPath: &*NSString::from_str(key)];
                }
            }
        }
        NSApplication::sharedApplication(objc2::MainThreadMarker::new().unwrap())
            .setDelegate(self.delegate.ivars().original.as_deref());
        if NATIVE_TERMINATION.get() {
            TERMINATION_DELEGATE.with(|original| {
                *original.borrow_mut() = self.delegate.ivars().original.clone();
            });
        }
    }
}
/// Call after worker, session, logs and instance lock have completed cleanup.
/// NSApplication's termination skips Rust destructors, so none may remain here.
pub fn finish_native_termination() {
    if NATIVE_TERMINATION.get() {
        let app = NSApplication::sharedApplication(objc2::MainThreadMarker::new().unwrap());
        app.terminate(None);
    }
}
