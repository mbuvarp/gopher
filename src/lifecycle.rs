//! Process diagnostics. Call only after acquiring the app's exclusive instance lock.
use crate::logging::LogWriter;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
struct Marker {
    pid: u32,
    started_at: String,
    version: String,
    ended_at: Option<String>,
    reason: Option<String>,
}

pub struct Session {
    path: PathBuf,
    marker: Marker,
    writer: LogWriter,
}

impl Session {
    pub fn start(directory: &Path, writer: LogWriter) -> Result<Self> {
        let path = directory.join("session.json");
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Marker>(&bytes) {
                Ok(previous) if previous.ended_at.is_none() => {
                    writer.diagnostic("WARN", json!({
                        "event": "previous_session_unclean",
                        "previous_pid": previous.pid,
                        "previous_started_at": previous.started_at,
                        "previous_version": previous.version,
                        "message": "The previous session has no completed shutdown record; the cause is unknown.",
                    }))?;
                }
                Ok(_) => (),
                Err(error) => writer.diagnostic(
                    "WARN",
                    json!({
                        "event": "session_marker_invalid", "error": error.to_string(),
                    }),
                )?,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error).context("Cannot read session marker"),
        }
        let session = Self {
            path,
            marker: Marker {
                pid: std::process::id(),
                started_at: chrono::Utc::now().to_rfc3339(),
                version: env!("CARGO_PKG_VERSION").into(),
                ended_at: None,
                reason: None,
            },
            writer,
        };
        session.persist()?;
        session.record(
            "INFO",
            "app_started",
            json!({"version": session.marker.version}),
        );
        Ok(session)
    }

    pub fn record(&self, level: &str, event: &str, mut fields: serde_json::Value) {
        fields["event"] = event.into();
        fields["pid"] = self.marker.pid.into();
        fields["session_started_at"] = self.marker.started_at.clone().into();
        if let Err(error) = self.writer.diagnostic(level, fields) {
            eprintln!("Gopher {event} diagnostic failed: {error}");
        }
    }

    /// Explicit completion only: unwinding, aborts and force kills leave the marker open.
    pub fn finish(mut self, reason: &str) -> Result<()> {
        self.writer.diagnostic(
            "INFO",
            json!({
                "event": "app_stopped", "reason": reason,
                "pid": self.marker.pid, "session_started_at": self.marker.started_at,
            }),
        )?;
        self.marker.ended_at = Some(chrono::Utc::now().to_rfc3339());
        self.marker.reason = Some(reason.into());
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        let temp = self.path.with_extension("tmp");
        let mut file = File::create(&temp)?;
        file.write_all(&serde_json::to_vec(&self.marker)?)?;
        file.sync_all()?;
        std::fs::rename(temp, &self.path)?;
        File::open(
            self.path
                .parent()
                .context("Session marker has no directory")?,
        )?
        .sync_all()?;
        Ok(())
    }
}

pub fn install_panic_hook(writer: LogWriter) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let message = info
            .payload()
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| info.payload().downcast_ref::<&str>().copied())
            .unwrap_or("Non-string panic payload");
        let fields = json!({
            "event": "rust_panic", "pid": std::process::id(),
            "thread": thread.name().unwrap_or("unnamed"),
            "thread_id": format!("{:?}", thread.id()),
            "message": message,
            "location": info.location().map(|location| location.to_string()),
            "backtrace": std::backtrace::Backtrace::force_capture().to_string(),
        });
        // The logging thread cannot acknowledge its own request. Keep stderr as a
        // fallback there, and for disk/queue failures. Never attempt crash recovery.
        if thread.name() == Some("gopher-logs") {
            eprintln!("Gopher logging thread panicked: {fields}");
        } else if let Err(error) = writer.diagnostic("ERROR", fields) {
            eprintln!("Gopher panic diagnostic failed: {error}");
        }
        previous(info);
    }));
}

#[cfg(unix)]
pub struct ShutdownSignals {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl ShutdownSignals {
    pub fn start(mut received: impl FnMut(&'static str) + Send + 'static) -> Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        // Register before returning, so a signal immediately after startup is handled.
        let (mut term, mut interrupt) = {
            let _entered = runtime.enter();
            (
                signal(SignalKind::terminate())?,
                signal(SignalKind::interrupt())?,
            )
        };
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("gopher-signals".into())
            .spawn(move || {
                runtime.block_on(async {
                    loop {
                        tokio::select! {
                            _ = &mut stopped => break,
                            _ = term.recv() => received("sigterm"),
                            _ = interrupt.recv() => received("sigint"),
                        }
                    }
                });
            })?;
        Ok(Self {
            stop: Some(stop),
            thread: Some(thread),
        })
    }
}

#[cfg(unix)]
impl Drop for ShutdownSignals {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
