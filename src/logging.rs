use anyhow::Result;
use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, SyncSender},
    thread::JoinHandle,
};
use tracing_subscriber::fmt::MakeWriter;

pub const MAX_LINES: usize = 10_000;
const MAX_BYTES: usize = 16_384;
enum Message {
    Record(Vec<u8>),
    Shutdown,
}
#[derive(Clone)]
pub struct LogWriter {
    sender: SyncSender<Message>,
}
pub struct RecordWriter {
    sender: SyncSender<Message>,
    buffer: Vec<u8>,
}
pub struct LogGuard {
    sender: SyncSender<Message>,
    thread: Option<JoinHandle<()>>,
}

impl<'a> MakeWriter<'a> for LogWriter {
    type Writer = RecordWriter;
    fn make_writer(&'a self) -> Self::Writer {
        RecordWriter {
            sender: self.sender.clone(),
            buffer: Vec::new(),
        }
    }
}
impl Write for RecordWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let available = (MAX_BYTES + 1).saturating_sub(self.buffer.len());
        self.buffer
            .extend_from_slice(&bytes[..bytes.len().min(available)]);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Drop for RecordWriter {
    fn drop(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        if self.buffer.len() > MAX_BYTES {
            self.buffer = format!("{{\"timestamp\":\"{}\",\"level\":\"WARN\",\"fields\":{{\"event\":\"oversized_log_record\",\"message\":\"Record exceeded 16 KiB and was omitted\"}}}}\n", chrono::Utc::now().to_rfc3339()).into_bytes();
        }
        // UI callbacks never wait for disk IO. A bounded queue also caps memory use.
        if self
            .sender
            .try_send(Message::Record(std::mem::take(&mut self.buffer)))
            .is_err()
        {
            eprintln!("Gopher log queue unavailable; one record was dropped");
        }
    }
}
impl Drop for LogGuard {
    fn drop(&mut self) {
        let _ = self.sender.send(Message::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn start(directory: &Path) -> Result<(LogWriter, LogGuard)> {
    std::fs::create_dir_all(directory)?;
    let path = directory.join("gopher.jsonl");
    let mut log = BoundedLog::open(path)?;
    let (sender, receiver) = mpsc::sync_channel(2048);
    let thread = std::thread::Builder::new()
        .name("gopher-logs".into())
        .spawn(move || {
            while let Ok(message) = receiver.recv() {
                match message {
                    Message::Shutdown => break,
                    Message::Record(record) => {
                        if let Err(error) = log.append(&record) {
                            eprintln!("Gopher log write failed: {error}");
                        }
                    }
                }
            }
        })?;
    Ok((
        LogWriter {
            sender: sender.clone(),
        },
        LogGuard {
            sender,
            thread: Some(thread),
        },
    ))
}

struct BoundedLog {
    path: PathBuf,
    lines: VecDeque<Vec<u8>>,
}
impl BoundedLog {
    fn open(path: PathBuf) -> io::Result<Self> {
        let mut log = Self {
            path,
            lines: VecDeque::new(),
        };
        match File::open(&log.path) {
            Ok(file) => {
                // Stream old logs, keeping memory bounded even after an abnormal shutdown.
                let mut reader = io::BufReader::new(file);
                while let Some(mut line) = bounded_line(&mut reader)? {
                    if line.len() > MAX_BYTES {
                        continue;
                    }
                    if serde_json::from_slice::<serde_json::Value>(&line).is_ok() {
                        if !line.ends_with(b"\n") {
                            line.push(b'\n');
                        }
                        log.lines.push_back(line);
                        if log.lines.len() > MAX_LINES {
                            log.lines.pop_front();
                        }
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
        log.replace()?;
        Ok(log)
    }
    fn append(&mut self, record: &[u8]) -> io::Result<()> {
        self.lines.push_back(record.to_vec());
        if self.lines.len() > MAX_LINES {
            while self.lines.len() > 9_000 {
                self.lines.pop_front();
            }
            self.replace()
        } else {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            file.write_all(record)
        }
    }
    fn replace(&self) -> io::Result<()> {
        let temp = self.path.with_extension("tmp");
        let mut file = File::create(&temp)?;
        for line in &self.lines {
            file.write_all(line)?;
        }
        file.sync_all()?;
        std::fs::rename(temp, &self.path)
    }
}

fn bounded_line(reader: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let mut seen = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(seen.then_some(line));
        }
        seen = true;
        let end = buffer.iter().position(|b| *b == b'\n').map(|i| i + 1);
        let count = end.unwrap_or(buffer.len());
        let keep = count.min((MAX_BYTES + 1).saturating_sub(line.len()));
        line.extend_from_slice(&buffer[..keep]);
        reader.consume(count);
        if end.is_some() {
            return Ok(Some(line));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_and_recovers_logs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let mut log = BoundedLog::open(path.clone()).unwrap();
        for i in 0..10_050 {
            log.append(format!("{{\"n\":{i}}}\n").as_bytes()).unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.lines().count() <= MAX_LINES);
        assert_eq!(text.lines().last(), Some("{\"n\":10049}"));
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{broken")
            .unwrap();
        let recovered = BoundedLog::open(path).unwrap();
        assert_eq!(recovered.lines.back().unwrap(), b"{\"n\":10049}\n");
    }

    #[test]
    fn oversized_records_remain_valid_json_and_flush_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let (writer, guard) = start(dir.path()).unwrap();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_ansi(false)
            .with_writer(writer)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(event="oversized",value=%"ø".repeat(MAX_BYTES));
            tracing::info!(event = "last_event");
        });
        drop(guard);
        let text = std::fs::read_to_string(dir.path().join("gopher.jsonl")).unwrap();
        let entries: Vec<serde_json::Value> = text
            .lines()
            .map(|line| {
                assert!(line.len() <= MAX_BYTES);
                serde_json::from_str(line).unwrap()
            })
            .collect();
        assert_eq!(entries[0]["fields"]["event"], "oversized_log_record");
        assert_eq!(entries[1]["fields"]["event"], "last_event");
    }
}
