use gopher::{
    config::Config,
    hotkeys::{Binding, COMMAND, HotkeyAction, Preferences},
    store::Store,
    worker::{self, Command, UiEvent},
};
use std::{
    sync::{Arc, mpsc},
    time::Duration,
};

#[test]
fn shortcuts_persist_independently_of_accounts_and_pr_cache() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(directory.path()).unwrap();
    let preferences = Preferences::default()
        .changed(
            HotkeyAction::OpenGopher,
            Some(Binding {
                key: 5,
                modifiers: COMMAND,
                label: "G".into(),
            }),
        )
        .unwrap()
        .changed(HotkeyAction::Next, None)
        .unwrap();
    store.save_hotkeys(&preferences).unwrap();
    store.set_viewer("first").unwrap();
    store.set_viewer("second").unwrap();
    store.retain(&Default::default()).unwrap();
    drop(store);
    let loaded = Store::open(directory.path()).unwrap().hotkeys().unwrap();
    assert_eq!(loaded, preferences);
    assert!(loaded.binding(HotkeyAction::Next).is_none());
}

#[test]
fn worker_confirms_shortcuts_only_after_persistence_and_reports_failures() {
    let directory = tempfile::tempdir().unwrap();
    let config = Config {
        gh_path: Some(directory.path().join("missing-gh")),
        ..Default::default()
    };
    let (tx, rx) = mpsc::channel();
    let worker = worker::start(
        directory.path().into(),
        config,
        Arc::new(move |event| {
            let _ = tx.send(event);
        }),
    )
    .unwrap();
    loop {
        if let UiEvent::HotkeysLoaded { preferences, error } =
            rx.recv_timeout(Duration::from_secs(5)).unwrap()
        {
            assert_eq!(preferences, Preferences::default());
            assert!(error.is_none());
            break;
        }
    }
    let preferences = Preferences::default()
        .changed(HotkeyAction::Next, None)
        .unwrap();
    worker
        .sender
        .send(Command::SaveHotkeys(preferences.clone()))
        .unwrap();
    loop {
        if let UiEvent::HotkeysSaved(result) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            result.unwrap();
            break;
        }
    }
    assert_eq!(
        Store::open(directory.path()).unwrap().hotkeys().unwrap(),
        preferences
    );
    let connection = rusqlite::Connection::open(directory.path().join("state.sqlite3")).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_hotkeys BEFORE INSERT ON metadata WHEN NEW.key='hotkeys' BEGIN SELECT RAISE(FAIL, 'Simulated disk failure'); END;").unwrap();
    worker
        .sender
        .send(Command::SaveHotkeys(Preferences::default()))
        .unwrap();
    loop {
        if let UiEvent::HotkeysSaved(result) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            assert!(result.unwrap_err().contains("Simulated disk failure"));
            break;
        }
    }
    assert_eq!(
        Store::open(directory.path()).unwrap().hotkeys().unwrap(),
        preferences
    );
    drop(worker);
}
