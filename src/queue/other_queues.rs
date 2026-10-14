//! The jobs that run in the queues of other layouts: another loadmaster
//! version's, beside this one's in the state directory. This queue does not
//! count them, so a machine runs beyond its capacity while both are in use
//! (for the minutes of an upgrade, or for as long as an agent runs an older
//! version); a loadmaster only tells of them. It reads nothing but the
//! columns every layout keeps (see [`SCHEMA`](super::SCHEMA)).

use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use super::QUEUE_DB_FILE;
use crate::process::{ProcessGroup, ProcessId};

/// The running jobs of the queue of another layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtherQueue {
    /// Its file name, as `queue-v2.db`.
    pub file: String,
    /// The tasks of its running jobs, in its order.
    pub running: Vec<String>,
}

/// A reader waits for a writer only in rare moments (as when the file is
/// first opened after a crash); a notice is not worth waiting long for.
const BUSY_TIMEOUT: Duration = Duration::from_secs(1);

/// The queues of other layouts in `dir` that have jobs running, by file
/// name. A file that cannot be read has none, as far as this goes.
pub fn running_in_other_queues(dir: &Path) -> Vec<OtherQueue> {
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
            let running = running_in(&dir.join(&file)).ok()?;
            (!running.is_empty()).then_some(OtherQueue { file, running })
        })
        .collect()
}

/// Whether `name` is that of a queue of some layout: `queue-v<N>.db`.
fn is_queue_file(name: &str) -> bool {
    name.strip_prefix("queue-v")
        .and_then(|rest| rest.strip_suffix(".db"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The tasks of the jobs running in the queue at `path`: those whose
/// loadmaster or task still runs, as for this queue's own (see
/// [`Job::is_live`](super::Job)).
fn running_in(path: &Path) -> rusqlite::Result<Vec<String>> {
    // Opened as its own version opens it, so that the last connection to
    // close it folds its write-ahead log into it and removes it, as a
    // read-only one would not; but never created, and its rows only read.
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    let mut stmt = conn.prepare(
        "SELECT task, owner_pid, owner_start, child_pgid, child_start
         FROM jobs WHERE state = 'running' ORDER BY id",
    )?;
    let jobs = stmt.query_map([], |row| {
        let task: String = row.get(0)?;
        let owner_pid: Option<u32> = row.get(1)?;
        let owner_start: Option<i64> = row.get(2)?;
        let child_pgid: Option<u32> = row.get(3)?;
        let child_start: Option<i64> = row.get(4)?;
        let owner = owner_pid.zip(owner_start).map(|(pid, start)| ProcessId {
            pid,
            start_time: start as u64,
        });
        let child = child_pgid.map(|pgid| ProcessGroup {
            pgid,
            leader_start_time: child_start.map(|t| t as u64),
        });
        let live = owner.is_some_and(|o| o.is_alive()) || child.is_some_and(|c| c.is_alive());
        Ok(live.then_some(task))
    })?;
    jobs.filter_map(Result::transpose).collect()
}
