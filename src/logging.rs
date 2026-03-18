//! Logging utilities for history file and trading events.

use std::fs::File;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

static HISTORY_FILE: OnceLock<Mutex<File>> = OnceLock::new();

/// Initialize the global history file writer (called by main binaries).
pub fn init_history_file(file: File) {
    HISTORY_FILE
        .set(Mutex::new(file))
        .expect("History file already initialized");
}

/// Write a message to stderr and the history file (if initialized).
pub fn log_to_history(message: &str) {
    eprint!("{}", message);
    let _ = std::io::stderr().flush();

    if let Some(file_mutex) = HISTORY_FILE.get() {
        if let Ok(mut file) = file_mutex.lock() {
            let _ = write!(file, "{}", message);
            let _ = file.flush();
        }
    }
}

/// Write a structured trading event with timestamp.
pub fn log_trading_event(event: &str) {
    use chrono::Utc;
    let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    log_to_history(&format!("[{}] {}\n", timestamp, event));
}
