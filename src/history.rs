//! An append-only log of jobs, one JSON object per line, for tuning weights.
//!
//! Each `loadmaster run` that got in line appends one record when it finishes
//! (or gives up). Nested runs never queue and are not logged.

use std::borrow::Cow;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// File name of the history log inside the state directory.
pub const HISTORY_FILE: &str = "history.jsonl";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The task ran; see `exit_code`.
    Exited,
    /// `--max-wait` elapsed; the job stays parked in line.
    GaveUp,
    /// A signal arrived while waiting.
    Interrupted,
    /// The command could not be started.
    SpawnFailed,
    /// loadmaster itself failed after getting in line (exit code 125).
    Error,
}

#[derive(Debug, Serialize)]
pub struct Record<'a> {
    pub job: i64,
    pub task: &'a str,
    pub weight: u32,
    /// Lossy for non-UTF-8 paths, so the record can always be written.
    pub dir: Cow<'a, str>,
    pub args: &'a [String],
    pub outcome: Outcome,
    /// When this invocation got in line, in Unix milliseconds.
    pub queued_at: u64,
    pub wait_ms: u64,
    pub run_ms: Option<u64>,
    /// What `loadmaster run` exited with.
    pub exit_code: Option<i32>,
}

pub fn append(state_dir: &Path, record: &Record) -> io::Result<()> {
    let mut line = serde_json::to_string(record)?;
    line.push('\n');
    // One write() per record; O_APPEND keeps concurrent writers' lines whole.
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir.join(HISTORY_FILE))?
        .write_all(line.as_bytes())
}

pub fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
