//! An append-only log of jobs, one JSON object per line, for tuning weights
//! and for `loadmaster report`.
//!
//! Each `loadmaster run` that got in line appends one record when it finishes
//! (or gives up). If it is killed first, whoever reaps its job appends one
//! with outcome `lost`. Nested runs never queue and are not logged.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize};

/// File name of the history log inside the state directory.
pub const HISTORY_FILE: &str = "history.jsonl";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// loadmaster was killed (SIGKILL, say) before it could log the result;
    /// written by the process that later reclaimed the job.
    Lost,
}

#[derive(Debug, Clone, Serialize)]
pub struct Record<'a> {
    pub job: i64,
    pub task: &'a str,
    pub weight: u32,
    /// The machine's capacity for this run (unknown only for lost jobs
    /// queued by an older version).
    pub capacity: Option<u32>,
    /// Whether the run was admitted without waiting (`measure_only`).
    pub measure_only: bool,
    /// Lossy for non-UTF-8 paths, so the record can always be written.
    pub dir: Cow<'a, str>,
    pub args: &'a [String],
    pub outcome: Outcome,
    /// When this invocation got in line, in Unix milliseconds.
    pub queued_at: u64,
    pub wait_ms: u64,
    pub run_ms: Option<u64>,
    /// CPU time (user plus system) the task and the descendants it waited
    /// for used; divided by `run_ms`, the cores it kept busy.
    pub cpu_ms: Option<u64>,
    /// The 1-minute load average when the task finished.
    pub load: Option<f64>,
    /// The signal that killed the task, if one did.
    pub signal: Option<i32>,
    /// What `loadmaster run` exited with.
    pub exit_code: Option<i32>,
    /// For a `lost` record: whether the task had started, which tells a run
    /// killed under load from a wait cut short.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started: Option<bool>,
    /// For a task that ran and exited on its own: whether its exit code is
    /// one of the task's `ok_exit_codes`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    /// The resources the task used, by name, so runs of one task with
    /// different settings can be told apart.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub uses: BTreeMap<String, u32>,
}

/// A record read back from the log. Fields that older versions did not write
/// are optional.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Entry {
    pub job: i64,
    pub task: String,
    pub weight: u32,
    #[serde(default)]
    pub dir: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub capacity: Option<u32>,
    #[serde(default)]
    pub measure_only: bool,
    pub outcome: Outcome,
    pub queued_at: u64,
    pub wait_ms: u64,
    pub run_ms: Option<u64>,
    #[serde(default)]
    pub cpu_ms: Option<u64>,
    #[serde(default)]
    pub load: Option<f64>,
    /// `None` if the record predates this field, `Some(None)` if the task
    /// was not killed by a signal.
    #[serde(default, deserialize_with = "present")]
    pub signal: Option<Option<i32>>,
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub started: Option<bool>,
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub uses: BTreeMap<String, u32>,
}

/// Tells a field that is present (even as `null`) from one that is missing.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
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

/// Reads every record of a history log, in file order, and counts the lines
/// that could not be parsed (a line cut short by a crash, say).
pub fn read(path: &Path) -> io::Result<(Vec<Entry>, usize)> {
    let bytes = std::fs::read(path)?;
    let mut entries = Vec::new();
    let mut unreadable = 0;
    for line in bytes.split(|&b| b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice(line) {
            Ok(entry) => entries.push(entry),
            Err(_) => unreadable += 1,
        }
    }
    Ok((entries, unreadable))
}

pub fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_read_back_and_bad_lines_are_counted() {
        let dir = tempfile::tempdir().unwrap();
        let args = ["a".to_owned()];
        let record = Record {
            job: 7,
            task: "test",
            weight: 4,
            capacity: Some(8),
            measure_only: true,
            dir: Cow::Borrowed("/repo"),
            args: &args,
            outcome: Outcome::Exited,
            queued_at: 1_000,
            wait_ms: 20,
            run_ms: Some(300),
            cpu_ms: Some(600),
            load: Some(2.5),
            signal: None,
            exit_code: Some(1),
            started: None,
            ok: Some(false),
            uses: BTreeMap::from([("db".to_owned(), 1)]),
        };
        append(dir.path(), &record).unwrap();
        append(
            dir.path(),
            &Record {
                outcome: Outcome::Lost,
                started: Some(false),
                ok: None,
                ..record.clone()
            },
        )
        .unwrap();
        let path = dir.path().join(HISTORY_FILE);
        let mut text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text.matches("\"started\"").count(),
            1,
            "only lost records say"
        );
        // A line from before `capacity` and `load` existed, and a torn line.
        text += r#"{"job":8,"task":"lint","weight":1,"dir":"/r","args":[],"outcome":"gave_up","queued_at":2000,"wait_ms":5,"run_ms":null,"exit_code":75}"#;
        text += "\n{\"job\":9,\"ta\n";
        std::fs::write(&path, text).unwrap();

        let (entries, unreadable) = read(&path).unwrap();
        assert_eq!(unreadable, 1);
        assert_eq!(
            entries[0],
            Entry {
                job: 7,
                task: "test".into(),
                weight: 4,
                dir: "/repo".into(),
                args: vec!["a".into()],
                capacity: Some(8),
                measure_only: true,
                outcome: Outcome::Exited,
                queued_at: 1_000,
                wait_ms: 20,
                run_ms: Some(300),
                cpu_ms: Some(600),
                load: Some(2.5),
                signal: Some(None),
                exit_code: Some(1),
                started: None,
                ok: Some(false),
                uses: BTreeMap::from([("db".to_owned(), 1)]),
            }
        );
        assert!(entries[2].uses.is_empty());
        assert_eq!(entries[1].started, Some(false));
        assert_eq!(entries[2].capacity, None);
        assert_eq!(entries[2].signal, None);
        assert_eq!(entries[2].started, None);
        assert!(!entries[2].measure_only);
        assert_eq!(entries[2].outcome, Outcome::GaveUp);
    }
}
