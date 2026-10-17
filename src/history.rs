//! An append-only log of jobs, one JSON object per line, for tuning weights
//! and for `loadmaster report`.
//!
//! Each `loadmaster run` that got in line appends one record when it finishes
//! (or gives up). If it is killed first, whoever reaps its job appends one
//! with outcome `lost`. `loadmaster cancel` appends one with outcome
//! `canceled` for each job it stops. Nested runs never queue and are not
//! logged.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

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
    /// Stopped with `loadmaster cancel`, which writes this record besides
    /// the one the run itself writes.
    Canceled,
}

/// A resource's limits as a run got in line with them: the machine-wide
/// capacity and the per-checkout limit, where there is one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_checkout: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Record<'a> {
    pub job: i64,
    pub task: &'a str,
    pub weight: u32,
    /// The machine's capacity for this run.
    pub capacity: u32,
    /// Whether the run was admitted without waiting (`measure_only`).
    pub measure_only: bool,
    /// Where it was run. Lossy for non-UTF-8 paths, so the record can
    /// always be written.
    pub dir: Cow<'a, str>,
    /// Its checkout: the resolved directory of the `.loadmaster.toml` it
    /// ran from, which `dir` may be inside of. Tasks of other checkouts are
    /// other workloads, though they may share a name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkout: Option<Cow<'a, str>>,
    /// What its checkout is one workload with: its repository and place in
    /// it (see [`crate::config::workload`]), shared by the repository's
    /// worktrees, which run the same tasks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workload: Option<String>,
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
    /// The machine's CPUs busy while the task ran, on average, in cores:
    /// inside the queue and outside it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_busy: Option<f64>,
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
    /// loadmaster passed the task a request to stop (its stop signal, for a
    /// SIGTERM or a cancel) before it exited.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stopped: bool,
    /// The cores the task started with, if fewer than its weight.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub granted: Option<u32>,
    /// The least cores it may start with, if fewer than its weight (its
    /// `min_weight`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_weight: Option<u32>,
    /// The resources the task used, by name, so runs of one task with
    /// different settings can be told apart.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub uses: BTreeMap<String, u32>,
    /// The limits of those resources, as it got in line with them: what a
    /// replay of the line needs.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub limits: BTreeMap<String, Limit>,
}

/// A record read back from the log, as [`Record`] writes it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Entry {
    pub job: i64,
    pub task: String,
    pub weight: u32,
    pub dir: String,
    /// Not in records of 0.15 and before.
    #[serde(default)]
    pub checkout: Option<String>,
    /// Not in records of 0.15 and before.
    #[serde(default)]
    pub workload: Option<String>,
    pub args: Vec<String>,
    pub capacity: u32,
    pub measure_only: bool,
    pub outcome: Outcome,
    pub queued_at: u64,
    pub wait_ms: u64,
    pub run_ms: Option<u64>,
    pub cpu_ms: Option<u64>,
    pub load: Option<f64>,
    pub machine_busy: Option<f64>,
    pub signal: Option<i32>,
    pub exit_code: Option<i32>,
    pub started: Option<bool>,
    pub ok: Option<bool>,
    #[serde(default)]
    pub stopped: bool,
    pub granted: Option<u32>,
    #[serde(default)]
    pub min_weight: Option<u32>,
    #[serde(default)]
    pub uses: BTreeMap<String, u32>,
    #[serde(default)]
    pub limits: BTreeMap<String, Limit>,
}

impl Entry {
    /// Its checkout, or, for a record of 0.15 and before, where it was run.
    pub fn checkout(&self) -> &str {
        self.checkout.as_deref().unwrap_or(&self.dir)
    }
    /// Whether it has what [`Record`] writes for its outcome. A record of
    /// an older format may lack it, and would be misread.
    fn is_whole(&self) -> bool {
        match self.outcome {
            Outcome::Exited => self.signal.is_some() || self.ok.is_some(),
            Outcome::Lost => self.started.is_some(),
            _ => true,
        }
    }
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
/// that are not records of this format (a line cut short by a crash, or one
/// an older version wrote).
pub fn read(path: &Path) -> io::Result<(Vec<Entry>, usize)> {
    Ok(parse(&std::fs::read(path)?))
}

/// [`read`] of the last `max_bytes` of the log at most: its latest records,
/// at a cost that does not grow with the log, which is never rotated.
pub fn read_recent(path: &Path, max_bytes: u64) -> io::Result<Vec<Entry>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let from = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(from))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    // Started inside a line: that part of it is no record.
    let whole = match from {
        0 => &bytes[..],
        _ => bytes
            .iter()
            .position(|&b| b == b'\n')
            .map_or(&[][..], |at| &bytes[at + 1..]),
    };
    Ok(parse(whole).0)
}

fn parse(bytes: &[u8]) -> (Vec<Entry>, usize) {
    let mut entries = Vec::new();
    let mut unreadable = 0;
    for line in bytes.split(|&b| b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Entry>(line) {
            Ok(entry) if entry.is_whole() => entries.push(entry),
            _ => unreadable += 1,
        }
    }
    (entries, unreadable)
}

pub fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_latest_records_are_read_from_the_end_of_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let args = [];
        for job in 1..=3 {
            let record = Record {
                job,
                task: "test",
                weight: 4,
                capacity: 8,
                measure_only: false,
                dir: Cow::Borrowed("/repo"),
                checkout: None,
                workload: None,
                args: &args,
                outcome: Outcome::Exited,
                queued_at: 1_000,
                wait_ms: 0,
                run_ms: Some(300),
                cpu_ms: None,
                load: None,
                machine_busy: None,
                signal: None,
                exit_code: Some(0),
                started: None,
                ok: Some(true),
                stopped: false,
                granted: None,
                min_weight: None,
                uses: BTreeMap::new(),
                limits: BTreeMap::new(),
            };
            append(dir.path(), &record).unwrap();
        }
        let path = dir.path().join(HISTORY_FILE);
        let line = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .len() as u64;
        // A line and a half: the cut one is dropped, the last is whole.
        let jobs = |max| -> Vec<i64> {
            read_recent(&path, max)
                .unwrap()
                .iter()
                .map(|e| e.job)
                .collect()
        };
        assert_eq!(jobs(line * 3 / 2), [3]);
        assert_eq!(jobs(u64::MAX), [1, 2, 3]);
    }

    #[test]
    fn records_read_back_and_other_lines_are_counted() {
        let dir = tempfile::tempdir().unwrap();
        let args = ["a".to_owned()];
        let record = Record {
            job: 7,
            task: "test",
            weight: 4,
            capacity: 8,
            measure_only: true,
            dir: Cow::Borrowed("/repo"),
            checkout: None,
            workload: None,
            args: &args,
            outcome: Outcome::Exited,
            queued_at: 1_000,
            wait_ms: 20,
            run_ms: Some(300),
            cpu_ms: Some(600),
            load: Some(2.5),
            machine_busy: Some(3.5),
            signal: None,
            exit_code: Some(1),
            started: None,
            ok: Some(false),
            stopped: false,
            granted: None,
            min_weight: None,
            uses: BTreeMap::from([("db".to_owned(), 1)]),
            limits: BTreeMap::new(),
        };
        append(dir.path(), &record).unwrap();
        for record in [
            Record {
                outcome: Outcome::Lost,
                started: Some(false),
                ok: None,
                ..record.clone()
            },
            // Killed by a signal, an exited task has no `ok`.
            Record {
                signal: Some(9),
                exit_code: Some(137),
                ok: None,
                uses: BTreeMap::new(),
                ..record.clone()
            },
        ] {
            append(dir.path(), &record).unwrap();
        }
        let path = dir.path().join(HISTORY_FILE);
        let mut text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text.matches("\"started\"").count(),
            1,
            "only lost records say"
        );
        // Lines of older formats: an exit without `ok`, a lost job without
        // `started`, a record without `capacity`; and a torn line.
        let exited = r#""job":8,"task":"lint","weight":1,"dir":"/r","args":[],"queued_at":2000,"wait_ms":5,"run_ms":9,"signal":null,"exit_code":0"#;
        text +=
            &format!("{{{exited},\"capacity\":8,\"measure_only\":false,\"outcome\":\"exited\"}}\n");
        text +=
            &format!("{{{exited},\"capacity\":8,\"measure_only\":false,\"outcome\":\"lost\"}}\n");
        text +=
            &format!("{{{exited},\"measure_only\":false,\"outcome\":\"exited\",\"ok\":true}}\n");
        text += "{\"job\":9,\"ta\n";
        std::fs::write(&path, text).unwrap();

        let (entries, unreadable) = read(&path).unwrap();
        assert_eq!(unreadable, 4);
        assert_eq!(
            entries[0],
            Entry {
                job: 7,
                task: "test".into(),
                weight: 4,
                dir: "/repo".into(),
                checkout: None,
                workload: None,
                args: vec!["a".into()],
                capacity: 8,
                measure_only: true,
                outcome: Outcome::Exited,
                queued_at: 1_000,
                wait_ms: 20,
                run_ms: Some(300),
                cpu_ms: Some(600),
                load: Some(2.5),
                machine_busy: Some(3.5),
                signal: None,
                exit_code: Some(1),
                started: None,
                ok: Some(false),
                stopped: false,
                granted: None,
                min_weight: None,
                uses: BTreeMap::from([("db".to_owned(), 1)]),
                limits: BTreeMap::new(),
            }
        );
        assert_eq!(entries[1].started, Some(false));
        assert_eq!((entries[2].signal, entries[2].ok), (Some(9), None));
        assert!(entries[2].uses.is_empty());
        assert_eq!(entries.len(), 3);
    }
}
