use gopher::{
    lifecycle::{self, Session},
    logging,
};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn records(directory: &Path) -> Vec<Value> {
    std::fs::read_to_string(directory.join("logs/gopher.jsonl"))
        .unwrap()
        .lines()
        .map(|line| {
            assert!(line.len() < 16_384);
            serde_json::from_str(line).unwrap()
        })
        .collect()
}

fn marker(directory: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(directory.join("session.json")).unwrap()).unwrap()
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn child(directory: &Path, mode: &str) -> Process {
    Process(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "lifecycle_child", "--nocapture"])
            .env("GOPHER_LIFECYCLE_TEST_DIR", directory)
            .env("GOPHER_LIFECYCLE_TEST_MODE", mode)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

fn wait_ready(process: &mut Process, directory: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !directory.join("ready").exists() {
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "Child exited before ready"
        );
        assert!(Instant::now() < deadline, "Child did not become ready");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_exit(process: &mut Process) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "Child did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
}

// Runs only in an isolated subprocess: hooks and signal handlers are process-global.
#[test]
fn lifecycle_child() {
    let Some(directory) = std::env::var_os("GOPHER_LIFECYCLE_TEST_DIR") else {
        return;
    };
    let directory = Path::new(&directory);
    let mode = std::env::var("GOPHER_LIFECYCLE_TEST_MODE").unwrap();
    let (writer, _guard) = logging::start(&directory.join("logs")).unwrap();
    if mode == "panic_exit" {
        // Exit from the prior hook: no unwinding, destructors, or log guard flush.
        std::panic::set_hook(Box::new(|_| std::process::exit(91)));
    }
    lifecycle::install_panic_hook(writer.clone());
    let session = Session::start(directory, writer.clone()).unwrap();
    if mode == "panic_exit" {
        panic!("isolated panic {}", "ø\n".repeat(20_000));
    }
    if mode == "kill" {
        std::fs::write(directory.join("ready"), "").unwrap();
        loop {
            std::thread::park();
        }
    }
    #[cfg(unix)]
    if mode == "signal" {
        let (sender, receiver) = std::sync::mpsc::channel();
        let _signals = lifecycle::ShutdownSignals::start(move |reason| {
            writer
                .diagnostic(
                    "INFO",
                    json!({"event": "shutdown_requested", "reason": reason}),
                )
                .unwrap();
            sender.send(reason).unwrap();
        })
        .unwrap();
        std::fs::write(directory.join("ready"), "").unwrap();
        let reason = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        session.finish(reason).unwrap();
        return;
    }
    session.finish("quit").unwrap();
}

#[test]
fn panic_is_durable_without_unwinding_and_restart_detects_unclean_session() {
    let directory = tempfile::tempdir().unwrap();
    let mut process = child(directory.path(), "panic_exit");
    assert_eq!(wait_exit(&mut process).code(), Some(91));
    assert!(marker(directory.path())["ended_at"].is_null());
    let rows = records(directory.path());
    let panic = rows
        .iter()
        .find(|r| r["fields"]["event"] == "rust_panic")
        .unwrap();
    assert!(
        panic["fields"]["message"]
            .as_str()
            .unwrap()
            .starts_with("isolated panic")
    );
    assert!(!panic["fields"]["backtrace"].as_str().unwrap().is_empty());
    assert!(
        panic["fields"]["location"]
            .as_str()
            .unwrap()
            .contains("lifecycle.rs")
    );
    let mut next = child(directory.path(), "clean");
    assert!(wait_exit(&mut next).success());
    assert!(
        records(directory.path())
            .iter()
            .any(|r| r["fields"]["event"] == "previous_session_unclean")
    );
    assert_eq!(marker(directory.path())["reason"], "quit");
    let mut third = child(directory.path(), "clean");
    assert!(wait_exit(&mut third).success());
    assert_eq!(
        records(directory.path())
            .iter()
            .filter(|r| r["fields"]["event"] == "previous_session_unclean")
            .count(),
        1
    );
}

#[test]
fn force_kill_is_detected_on_next_launch_without_inventing_a_cause() {
    let directory = tempfile::tempdir().unwrap();
    let mut process = child(directory.path(), "kill");
    wait_ready(&mut process, directory.path());
    process.0.kill().unwrap();
    assert!(!wait_exit(&mut process).success());
    let mut next = child(directory.path(), "clean");
    assert!(wait_exit(&mut next).success());
    let rows = records(directory.path());
    let warning = rows
        .iter()
        .find(|r| r["fields"]["event"] == "previous_session_unclean")
        .unwrap();
    assert_eq!(warning["fields"]["previous_pid"], process.0.id());
    assert!(
        warning["fields"]["message"]
            .as_str()
            .unwrap()
            .contains("cause is unknown")
    );
}

#[cfg(unix)]
#[test]
fn termination_signals_are_logged_and_allow_clean_completion() {
    for (signal, reason) in [("-TERM", "sigterm"), ("-INT", "sigint")] {
        let directory = tempfile::tempdir().unwrap();
        let mut process = child(directory.path(), "signal");
        wait_ready(&mut process, directory.path());
        assert!(
            Command::new("/bin/kill")
                .args([signal, &process.0.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        assert!(wait_exit(&mut process).success());
        let rows = records(directory.path());
        assert_eq!(rows[1]["fields"]["event"], "shutdown_requested");
        assert_eq!(rows[1]["fields"]["reason"], reason);
        assert_eq!(rows[2]["fields"]["event"], "app_stopped");
        assert_eq!(marker(directory.path())["reason"], reason);
    }
}

#[test]
fn invalid_configuration_is_logged_and_cli_diagnostics_do_not_change_the_session() {
    let home = tempfile::tempdir().unwrap();
    let directory = home.path().join(gopher::identity::DATA_DIRECTORY);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("config.toml"), "poll_seconds = 0").unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_gopher"))
            .args(args)
            .env("HOME", home.path())
            .output()
            .unwrap()
    };
    assert!(!run(&[]).status.success());
    assert!(
        records(&directory)
            .iter()
            .any(|r| r["fields"]["event"] == "app_failed")
    );
    assert_eq!(marker(&directory)["reason"], "error");
    let before = std::fs::read(directory.join("session.json")).unwrap();
    assert!(!run(&["doctor"]).status.success());
    assert_eq!(
        std::fs::read(directory.join("session.json")).unwrap(),
        before
    );
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .open(directory.join("gopher.lock"))
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&lock).unwrap();
    let output = run(&[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already running"));
    assert_eq!(
        std::fs::read(directory.join("session.json")).unwrap(),
        before
    );
}
