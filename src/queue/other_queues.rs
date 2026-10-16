//! The jobs in the queues of other layouts: those of loadmaster 0.11 and
//! before, beside this one's in the state directory (0.12 and later keep
//! `queue-v4.db`; see `docs/DECISIONS.md`, "The queue file"). This queue
//! does not count them, so a machine runs beyond its capacity while both are
//! in use; a loadmaster only tells of them. It reads nothing but the columns
//! every layout keeps (see [`SCHEMA`](super::SCHEMA)).

use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use super::QUEUE_DB_FILE;
use crate::process::{ProcessGroup, ProcessId};

/// The queue of another layout, and the jobs it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtherQueue {
    /// Its file name, as `queue-v2.db`.
    pub file: String,
    /// Its jobs, if it could be read.
    pub jobs: Option<OtherJobs>,
}

/// The tasks of another queue's jobs, in its order: those that run, those
/// that wait, and those that keep a parked place, which they may resume (when
/// a place expires is not among the columns every layout keeps).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OtherJobs {
    pub running: Vec<String>,
    pub waiting: Vec<String>,
    pub parked: Vec<String>,
}

impl OtherJobs {
    pub fn count(&self) -> usize {
        self.running.len() + self.waiting.len() + self.parked.len()
    }
}

/// A reader waits for a writer only in rare moments (as when the file is
/// first opened after a crash); a notice is not worth waiting long for.
const BUSY_TIMEOUT: Duration = Duration::from_secs(1);

/// The queues of other layouts in `dir` that hold jobs, or could not be read
/// (they may), by file name.
pub fn jobs_in_other_queues(dir: &Path) -> Vec<OtherQueue> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<String> = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name != QUEUE_DB_FILE && is_queue_file(name))
        .collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|file| {
            let jobs = jobs_in(&dir.join(&file)).ok();
            jobs.as_ref()
                .is_none_or(|jobs| jobs.count() > 0)
                .then_some(OtherQueue { file, jobs })
        })
        .collect()
}

/// Whether `name` is that of a queue of some layout: `queue-v<N>.db`.
fn is_queue_file(name: &str) -> bool {
    name.strip_prefix("queue-v")
        .and_then(|rest| rest.strip_suffix(".db"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The jobs in the queue at `path` that someone still holds, as for this
/// queue's own (see [`Job::is_live`](super::Job)): a running one while its
/// loadmaster or task runs, a waiting one while its loadmaster does; every
/// parked one.
fn jobs_in(path: &Path) -> rusqlite::Result<OtherJobs> {
    // Opened as its own version opens it, so that the last connection to
    // close it folds its write-ahead log into it and removes it, as a
    // read-only one would not; but never created, and its rows only read.
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    let mut stmt = conn.prepare(
        "SELECT task, state, owner_pid, owner_start, child_pgid, child_start
         FROM jobs ORDER BY id",
    )?;
    let rows = stmt.query_map([], |row| {
        let owner_pid: Option<u32> = row.get(2)?;
        let owner_start: Option<i64> = row.get(3)?;
        let child_pgid: Option<u32> = row.get(4)?;
        let child_start: Option<i64> = row.get(5)?;
        let owner = owner_pid.zip(owner_start).map(|(pid, start)| ProcessId {
            pid,
            start_time: start as u64,
        });
        let child = child_pgid.map(|pgid| ProcessGroup {
            pgid,
            leader_start_time: child_start.map(|t| t as u64),
        });
        let owner_alive = owner.is_some_and(|o| o.is_alive());
        let state: String = row.get(1)?;
        let live = match state.as_str() {
            "running" => owner_alive || child.is_some_and(|c| c.is_alive()),
            "waiting" => owner_alive,
            _ => true,
        };
        Ok(live.then(|| (row.get::<_, String>(0), state)))
    })?;
    let mut jobs = OtherJobs::default();
    for row in rows {
        let Some((task, state)) = row? else { continue };
        let task = task?;
        match state.as_str() {
            "running" => jobs.running.push(task),
            "waiting" => jobs.waiting.push(task),
            _ => jobs.parked.push(task),
        }
    }
    Ok(jobs)
}
