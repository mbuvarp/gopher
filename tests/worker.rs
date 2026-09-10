use gopher::{
    config::Config,
    worker::{self, Command, UiEvent},
};
use std::{
    sync::{Arc, mpsc},
    time::Duration,
};

#[test]
fn missing_cli_notifies_once_and_worker_shuts_down_cleanly() {
    let directory = tempfile::tempdir().unwrap();
    let config = Config {
        gh_path: Some(directory.path().join("missing-gh")),
        ..Default::default()
    };
    let (sender, receiver) = mpsc::channel();
    let worker = worker::start(
        directory.path().into(),
        config,
        Arc::new(move |event| {
            let _ = sender.send(event);
        }),
    )
    .unwrap();
    let mut notified = None;
    let mut saw_refresh = false;
    loop {
        match receiver.recv_timeout(Duration::from_secs(5)).unwrap() {
            UiEvent::Notify { body, .. } => {
                assert!(notified.replace(body).is_none());
            }
            UiEvent::Updated { loading: true, .. } => saw_refresh = true,
            UiEvent::Updated {
                error: Some(error),
                loading: false,
                ..
            } => {
                assert!(saw_refresh);
                assert_eq!(Some(error), notified);
                break;
            }
            _ => (),
        }
    }
    worker
        .sender
        .send(Command::PollComplete(Err(notified.unwrap())))
        .unwrap();
    match receiver.recv_timeout(Duration::from_secs(5)).unwrap() {
        UiEvent::Updated { error: Some(_), .. } => (),
        _ => panic!("An unchanged missing-CLI error must not emit another notification"),
    }
    let start = std::time::Instant::now();
    drop(worker);
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn notification_actions_acknowledge_only_their_update_and_open_waits_for_browser() {
    use gopher::{model::Snapshot, store::Store, worker::transition};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let mut pr = transition(
        Snapshot {
            id: "PR_1".into(),
            url: "https://github.com/owner/repo/pull/1".into(),
            ..Default::default()
        },
        None,
        None,
        100,
        0,
    );
    let old_update = pr.update_id.clone();
    let old_notice = store.notification(&pr).unwrap().unwrap();
    pr.update_id = "new-update".into();
    let new_notice = store.notification(&pr).unwrap().unwrap();
    store.save(&pr).unwrap();
    drop(store);
    let (tx, rx) = mpsc::channel();
    let worker = worker::start(
        directory.path().into(),
        Config {
            gh_path: Some(directory.path().join("missing-gh")),
            ..Default::default()
        },
        Arc::new(move |event| {
            tx.send(event).unwrap();
        }),
    )
    .unwrap();
    loop {
        if matches!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            UiEvent::Updated { error: Some(_), .. }
        ) {
            break;
        }
    }
    worker
        .sender
        .send(Command::NotificationAction {
            id: old_notice.clone(),
            open: true,
            reveal: false,
        })
        .unwrap();
    assert!(
        matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), UiEvent::Open {url,update,..} if url==pr.snapshot.url && update==old_update)
    );
    assert!(
        matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), UiEvent::Updated {prs,..} if prs[0].acknowledged.is_none())
    );
    for (notice, expected_ack, reveal) in [
        (old_notice.clone(), None, false),
        (old_notice, None, true),
        (new_notice, Some(pr.update_id.clone()), true),
    ] {
        worker
            .sender
            .send(Command::NotificationAction {
                id: notice.clone(),
                open: false,
                reveal,
            })
            .unwrap();
        // Acknowledge must dismiss the selected notice without emitting an Open event.
        assert!(
            matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), UiEvent::DismissNotifications(ids) if ids==vec![notice])
        );
        assert!(
            matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), UiEvent::Updated {prs,..} if prs[0].acknowledged==expected_ack)
        );
        if reveal {
            assert!(
                matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), UiEvent::ShowPopover {pr: Some(id)} if id=="PR_1")
            );
        }
    }
    drop(worker);
    assert_eq!(
        Store::open(directory.path()).unwrap().load().unwrap()[0].acknowledged,
        Some(pr.update_id)
    );
}

#[test]
fn notification_without_a_saved_target_still_opens_the_popover() {
    let directory = tempfile::tempdir().unwrap();
    let (tx, rx) = mpsc::channel();
    let worker = worker::start(
        directory.path().into(),
        Config {
            gh_path: Some(directory.path().join("missing-gh")),
            ..Default::default()
        },
        Arc::new(move |event| {
            let _ = tx.send(event);
        }),
    )
    .unwrap();
    loop {
        if matches!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            UiEvent::Updated {
                error: Some(_),
                loading: false,
                ..
            }
        ) {
            break;
        }
    }
    worker
        .sender
        .send(Command::NotificationAction {
            id: "service-error".into(),
            open: false,
            reveal: true,
        })
        .unwrap();
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        UiEvent::Updated { .. }
    ));
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        UiEvent::ShowPopover { pr: None }
    ));
    drop(worker);
}

#[test]
fn first_ui_state_contains_persisted_catalogue_and_assignments_without_github() {
    use gopher::{
        actions::LabelCatalogue,
        model::{PrLabel, Snapshot},
        store::Store,
        worker::transition,
    };
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(directory.path()).unwrap();
    store.set_viewer("test").unwrap();
    let bug = PrLabel {
        name: "bug".into(),
        color: "ff0000".into(),
    };
    store
        .save_label_catalogue(
            "test",
            "owner/repo",
            &LabelCatalogue {
                labels: vec![
                    bug.clone(),
                    PrLabel {
                        name: "ready".into(),
                        color: "00ff00".into(),
                    },
                ],
                fetched_at: chrono::Utc::now().timestamp(),
            },
        )
        .unwrap();
    store
        .save(&transition(
            Snapshot {
                id: "PR_cached".into(),
                repo: "owner/repo".into(),
                number: 1,
                open: true,
                head: "head".into(),
                labels: vec![bug],
                ..Default::default()
            },
            None,
            None,
            chrono::Utc::now().timestamp(),
            0,
        ))
        .unwrap();
    drop(store);
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = gopher::worker::start(
        directory.path().into(),
        gopher::config::Config {
            gh_path: Some(directory.path().join("missing-gh")),
            ..Default::default()
        },
        std::sync::Arc::new(move |event| {
            let _ = tx.send(event);
        }),
    )
    .unwrap();
    let event = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    let gopher::worker::UiEvent::ActionsChanged(state) = event else {
        panic!("Expected cached actions as the first UI event")
    };
    let labels = &state.labels["PR_cached"];
    assert!(labels.catalogue_ready);
    assert!(!labels.loading);
    assert_eq!(labels.items.len(), 2);
    assert!(labels.items[0].selected);
    assert!(!labels.items[1].selected);
    drop(worker);
}
