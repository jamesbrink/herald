//! Simple file logging for Herald.
//!
//! Provides a global file logger that writes timestamped log lines to
//! `{data_dir}/logs/herald.log`. Log files are rotated when they exceed
//! a configurable size (default 10 MB), keeping a small number of backups.
//!
//! Usage:
//!   logging::init(&data_dir);   // call once at startup
//!   hlog!("[cron] Job fired");  // writes to both stderr and the log file

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tokio::sync::Mutex;

/// Global logger instance.
static LOGGER: OnceLock<Mutex<FileLogger>> = OnceLock::new();

/// Maximum log file size before rotation (10 MB).
const MAX_LOG_SIZE: u64 = 10 * 1024 * 1024;

/// Number of rotated backups to keep.
const MAX_BACKUPS: usize = 3;

struct FileLogger {
    path: PathBuf,
    file: File,
}

impl FileLogger {
    fn new(log_dir: &Path) -> std::io::Result<Self> {
        fs::create_dir_all(log_dir)?;
        let path = log_dir.join("herald.log");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self { path, file })
    }

    fn write_line(&mut self, line: &str) {
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
        let formatted = format!("{} {}\n", ts, line);
        let _ = self.file.write_all(formatted.as_bytes());
        let _ = self.file.flush();

        // Check for rotation
        if let Ok(meta) = fs::metadata(&self.path) {
            if meta.len() > MAX_LOG_SIZE {
                self.rotate();
            }
        }
    }

    fn rotate(&mut self) {
        // Close current file (will reopen after rotation)
        let _ = self.file.flush();

        // Shift existing backups: .3 -> delete, .2 -> .3, .1 -> .2, current -> .1
        for i in (1..MAX_BACKUPS).rev() {
            let from = self.path.with_extension(format!("log.{}", i));
            let to = self.path.with_extension(format!("log.{}", i + 1));
            let _ = fs::rename(&from, &to);
        }
        let backup = self.path.with_extension("log.1");
        let _ = fs::rename(&self.path, &backup);

        // Open a fresh file
        if let Ok(file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            self.file = file;
        }
    }
}

/// Initialize the global file logger. Call once at startup.
///
/// Logs will be written to `{data_dir}/logs/herald.log`.
/// If initialization fails (e.g. permission error), a warning is printed
/// to stderr and logging silently degrades to stderr-only.
pub fn init(data_dir: &Path) {
    let log_dir = data_dir.join("logs");
    match FileLogger::new(&log_dir) {
        Ok(logger) => {
            let _ = LOGGER.set(Mutex::new(logger));
            eprintln!(
                "[init] file logging: {}",
                log_dir.join("herald.log").display()
            );
        }
        Err(e) => {
            eprintln!(
                "[init] warning: could not initialize file logging at {}: {}",
                log_dir.display(),
                e
            );
        }
    }
}

/// Write a line to the log file (non-blocking best-effort).
/// Called by the `hlog!` macro — you should not need to call this directly.
pub fn log_line(line: &str) {
    if let Some(logger) = LOGGER.get() {
        // Use try_lock to avoid blocking the async runtime.
        // If the lock is contended, we skip file logging for this line
        // (stderr still gets it via the macro).
        if let Ok(mut guard) = logger.try_lock() {
            guard.write_line(line);
        }
    }
}

/// Log to both stderr and the log file.
///
/// Usage is identical to `eprintln!`:
/// ```ignore
/// hlog!("[cron] Running job '{}'", job.name);
/// ```
#[macro_export]
macro_rules! hlog {
    // Empty call — just prints a blank line to stderr (no file logging for blank lines)
    () => {{
        eprintln!();
    }};
    ($($arg:tt)+) => {{
        let msg = format!($($arg)+);
        eprintln!("{}", msg);
        $crate::logging::log_line(&msg);
    }};
}
