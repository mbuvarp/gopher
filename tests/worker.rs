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
        })
        .unwrap();
    assert!(
        matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), UiEvent::Open {url,update,..} if url==pr.snapshot.url && update==old_update)
    );
    assert!(
        matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), UiEvent::Updated {prs,..} if prs[0].acknowledged.is_none())
    );
    for (notice, expected_ack) in [(old_notice, None), (new_notice, Some(pr.update_id.clone()))] {
        worker
            .sender
            .send(Command::NotificationAction {
                id: notice.clone(),
                open: false,
            })
            .unwrap();
        // Acknowledge must dismiss the selected notice without emitting an Open event.
        assert!(
            matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), UiEvent::DismissNotifications(ids) if ids==vec![notice])
        );
        assert!(
            matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), UiEvent::Updated {prs,..} if prs[0].acknowledged==expected_ack)
        );
    }
    drop(worker);
    assert_eq!(
        Store::open(directory.path()).unwrap().load().unwrap()[0].acknowledged,
        Some(pr.update_id)
    );
}
