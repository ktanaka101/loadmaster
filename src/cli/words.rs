//! What `loadmaster run` tells the agent that runs it, in the words the
//! guide documents: every line on stderr starts with `loadmaster: `, a job
//! is named `#ID `task``, and `loadmaster: next: ` gives the action to take.

use crate::*;
use loadmaster::queue::{Cause, Snapshot};
use loadmaster::usual::Usual;

/// "#42 `test`".
pub(crate) fn who(id: JobId, task: &str) -> String {
    format!("#{id} `{task}`")
}

/// The command that resumes the place of a run that gave up: the same task
/// and extra arguments, from the same directory.
pub(crate) fn rerun(task: &str, max_wait: Option<u64>, args: &[String]) -> String {
    let mut words = vec!["loadmaster".to_owned(), "run".to_owned(), task.to_owned()];
    if let Some(secs) = max_wait {
        words.extend(["--max-wait".to_owned(), secs.to_string()]);
    }
    if !args.is_empty() {
        words.push("--".to_owned());
        words.extend(args.iter().cloned());
    }
    display_command(&words, &[])
}

/// What a waiting job waits for and who holds it: "for 4 cores (8 of 8 in
/// use); running: #41 `e2e` for 1m02s (usually 3m10s)".
pub(crate) fn waits_for(wait: &Wait, snapshot: Option<&Snapshot>, usual: &Usual) -> String {
    let mut text = format!("for {}", wait.waits_for());
    let Some(snapshot) = snapshot else {
        return text;
    };
    // The jobs ahead that hold what it waits for, each once.
    let mut holders: Vec<JobId> = Vec::new();
    for shortage in &wait.short {
        if let Cause::InUse { by, .. } = &shortage.cause {
            for job in by {
                if !holders.contains(&job.id) {
                    holders.push(job.id);
                }
            }
        }
    }
    let now = SystemTime::now();
    let running: Vec<String> = holders
        .iter()
        .filter_map(|id| snapshot.jobs.iter().find(|job| job.id == *id))
        .filter(|job| job.state == JobState::Running)
        .map(|job| {
            let mut part = who(job.id, &job.task);
            if let Some(ran) = job.started_at.and_then(|t| now.duration_since(t).ok()) {
                part += &format!(" for {}", format_duration(ran));
            }
            if let Some(usual) = usual.of_job(job) {
                part += &format!(" (usually {})", format_duration(usual.median));
            }
            part
        })
        .collect();
    const SHOWN: usize = 3;
    if !running.is_empty() {
        let more = running.len().saturating_sub(SHOWN);
        text += "; running: ";
        text += &running[..running.len().min(SHOWN)].join(", ");
        if more > 0 {
            text += &format!(" and {more} more");
        }
    }
    text
}

/// What a wait line is about: its place and, for each shortage, what and
/// who. A line is said again when this changes; the numbers in use change
/// too often to count.
pub(crate) fn wait_key(wait: &Wait) -> String {
    let parts: Vec<String> = wait
        .short
        .iter()
        .map(|s| match &s.cause {
            Cause::InUse { by, .. } => {
                let ids: Vec<String> = by.iter().map(|j| j.id.to_string()).collect();
                format!("{:?} in use by {}", s.need, ids.join(","))
            }
            Cause::BesideStart { start, .. } => format!("{:?} beside {}", s.need, start.id),
        })
        .collect();
    format!("{} ahead; {}", wait.ahead, parts.join("; "))
}

/// How a run ended, for its last line. `ran` is how long the task ran,
/// `waited` how long it waited first.
pub(crate) fn ended(record: &Record, code: i32, ran: Option<Duration>) -> String {
    let who = who(record.job, record.task);
    let waited = Duration::from_millis(record.wait_ms);
    let waited = if waited >= Duration::from_secs(1) {
        format!("; waited {}", format_duration(waited))
    } else {
        String::new()
    };
    let ran = ran.map_or(String::new(), |d| format!(" in {}", format_duration(d)));
    match record.outcome {
        Outcome::Exited => match (record.signal, record.ok) {
            (Some(signal), _) => format!(
                "{who} was killed by {}{ran} (exit {code}{waited})",
                runner::signal_name(signal)
            ),
            (None, _) if record.stopped => {
                format!("{who} stopped as asked{ran} (exit {code}{waited})")
            }
            (None, Some(true)) => format!("{who} passed{ran} (exit {code}{waited})"),
            (None, _) => format!("{who} failed{ran} (exit {code}{waited})"),
        },
        Outcome::Interrupted => {
            format!("{who} left the line without running (exit {code}{waited})")
        }
        Outcome::SpawnFailed => format!("{who} could not start (exit {code})"),
        // Said where they happen.
        Outcome::GaveUp | Outcome::Error | Outcome::Lost | Outcome::Canceled => String::new(),
    }
}
