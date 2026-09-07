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
    loop {
        match receiver.recv_timeout(Duration::from_secs(5)).unwrap() {
            UiEvent::Notify { body, .. } => {
                assert!(notified.replace(body).is_none());
            }
            UiEvent::Updated {
                error: Some(error), ..
            } => {
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
