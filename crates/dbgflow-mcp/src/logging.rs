use dbgflow_core::logging::{LogEvent, LogSink};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const LOG_PREFIX: &str = "dbgflow-";
const LOG_SUFFIX: &str = ".jsonl";

#[derive(Debug)]
pub struct FileLogSink {
    log_dir: PathBuf,
    retention: Duration,
    lock: Mutex<()>,
}

impl FileLogSink {
    pub fn new(log_dir: impl Into<PathBuf>, retention_days: u64) -> std::io::Result<Self> {
        let log_dir = log_dir.into();
        fs::create_dir_all(&log_dir)?;
        let sink = Self {
            log_dir,
            retention: Duration::from_secs(retention_days.saturating_mul(24 * 60 * 60)),
            lock: Mutex::new(()),
        };
        sink.cleanup_old_logs(SystemTime::now())?;
        Ok(sink)
    }

    fn append(&self, event: &LogEvent) -> std::io::Result<()> {
        let _guard = self.lock.lock().ok();
        self.cleanup_old_logs(SystemTime::now())?;
        let path = self.log_dir.join(log_file_name(event.timestamp_unix_ms));
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        serde_json::to_writer(&mut file, event)?;
        writeln!(file)
    }

    fn cleanup_old_logs(&self, now: SystemTime) -> std::io::Result<()> {
        let cutoff = now.checked_sub(self.retention).unwrap_or(UNIX_EPOCH);
        let entries = match fs::read_dir(&self.log_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };

        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if !is_managed_log_file(&path) {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            if modified < cutoff {
                let _ = fs::remove_file(path);
            }
        }
        Ok(())
    }
}

impl LogSink for FileLogSink {
    fn log(&self, event: LogEvent) {
        let _ = self.append(&event);
    }
}

fn is_managed_log_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    (name.starts_with(LOG_PREFIX) && name.ends_with(LOG_SUFFIX)) || name == "service.log"
}

fn log_file_name(timestamp_unix_ms: u64) -> String {
    let days = (timestamp_unix_ms / 1000 / 86_400) as i64;
    let (year, month, day) = civil_from_days(days);
    format!("{LOG_PREFIX}{year:04}-{month:02}-{day:02}{LOG_SUFFIX}")
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::{log_file_name, FileLogSink};
    use dbgflow_core::logging::{LogEvent, LogLevel, LogSink};
    use std::fs;
    use std::time::{Duration, SystemTime};

    #[test]
    fn log_file_name_uses_iso_date() {
        assert_eq!(log_file_name(0), "dbgflow-1970-01-01.jsonl");
        assert_eq!(log_file_name(1_780_560_000_000), "dbgflow-2026-06-04.jsonl");
    }

    #[test]
    fn writes_jsonl_log_event() {
        let root = test_dir("file-log-write");
        let sink = FileLogSink::new(&root, 7).expect("create sink");

        sink.log(LogEvent::new(LogLevel::Info, "test", "event").field("answer", 42));

        let entries = fs::read_dir(&root)
            .expect("read log dir")
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        let path = entries[0].as_ref().expect("entry").path();
        let content = fs::read_to_string(path).expect("read log");
        assert!(content.contains("\"component\":\"test\""));
        assert!(content.contains("\"event\":\"event\""));
    }

    #[test]
    fn retention_removes_only_managed_old_logs() {
        let root = test_dir("file-log-retention");
        fs::create_dir_all(&root).expect("create log dir");
        let old_log = root.join("dbgflow-2000-01-01.jsonl");
        let keep = root.join("keep.txt");
        fs::write(&old_log, "").expect("old log");
        fs::write(&keep, "").expect("keep file");

        let sink = FileLogSink {
            log_dir: root.clone(),
            retention: Duration::from_secs(0),
            lock: Default::default(),
        };
        sink.cleanup_old_logs(SystemTime::now())
            .expect("cleanup logs");

        assert!(!old_log.exists());
        assert!(keep.exists());
    }

    fn test_dir(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create test dir");
        root
    }
}
