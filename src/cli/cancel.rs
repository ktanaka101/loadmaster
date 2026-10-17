//! `loadmaster cancel` and `loadmaster clean`.

use crate::*;

pub(crate) fn clean() -> Result<i32> {
    let (_, queue) = open_queue()?;
    let reaped = queue.reap()?;
    let mut out = String::new();
    if reaped.is_empty() {
        out += "nothing to reclaim\n";
    } else {
        out += &format!("reclaimed {} job(s):\n", reaped.len());
        for job in &reaped {
            out += &format!(
                "  #{} {} (weight {}) in {}\n",
                job.id,
                job.task,
                job.weight,
                job.dir.display()
            );
        }
    }
    print(&out);
    Ok(0)
}

/// After this long, `cancel` says which jobs are still stopping.
const CANCEL_HINT_AFTER: Duration = Duration::from_secs(10);

/// The signal a cancel request stands for: the task's stop signal, or
/// SIGKILL.
pub(crate) fn cancel_signal(kill: bool, task: &Task) -> i32 {
    if kill {
        libc::SIGKILL
    } else {
        task.stop_signal
    }
}

pub(crate) fn cancel(args: CancelArgs) -> Result<i32> {
    let (_, queue) = open_queue()?;
    // The job this runs in, if any: canceling it would end this command.
    let own = env::var(HELD_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<JobId>().ok());
    let ids: Vec<JobId> = if args.here {
        let cwd =
            env::current_dir().map_err(|e| Error::io("cannot read the current directory", e))?;
        let config = RepoConfig::locate(&cwd)?;
        let here = config.parent().and_then(|dir| fs::canonicalize(dir).ok());
        queue
            .snapshot()?
            .jobs
            .into_iter()
            .filter(|job| here.as_ref() == Some(&job.checkout))
            .map(|job| job.id)
            .collect()
    } else {
        // Each once: a second SIGTERM would reach the task again.
        let mut named = std::collections::BTreeSet::new();
        args.ids
            .iter()
            .copied()
            .filter(|&id| named.insert(id))
            .collect()
    };
    let mut out = String::new();
    // Each job asked to stop, and what it was doing for how long.
    let mut stopping: Vec<(Job, String)> = Vec::new();
    for id in ids {
        if Some(id) == own {
            say!("loadmaster: not canceling job #{id}: this command runs in it");
            continue;
        }
        let job = match queue.cancel(id, args.kill)? {
            Canceled::NotFound if args.here => continue,
            Canceled::NotFound => {
                say!("loadmaster: job #{id} is not in the queue");
                continue;
            }
            Canceled::DroppedPlace(job) => {
                out += &format!("dropped the kept place of #{id} `{}`\n", job.task);
                continue;
            }
            Canceled::Requested(job) | Canceled::Signaled(job) => job,
        };
        let (what, since) = match job.state {
            JobState::Running => ("running", job.started_at),
            _ => ("waiting", Some(job.enqueued_at)),
        };
        let duration = since
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .map_or(String::new(), |d| format!(" for {}", format_duration(d)));
        stopping.push((job, format!("{what}{duration}")));
    }
    if stopping.is_empty() && out.is_empty() {
        print("no jobs to cancel\n");
        return Ok(0);
    }
    // Gone once its loadmaster has left the line, or its task has exited and
    // freed the slot. The request in the queue records what was asked and
    // what was done, so nothing is sent twice (see `Queue::stop`). A job's
    // loadmaster acts on it; one stopped with Ctrl-Z acts on nothing, so it
    // is continued. A job whose loadmaster is gone has its task signalled.
    // A job can change meanwhile: one that gave up waiting (exit code 75)
    // keeps its place, which is dropped rather than left to expire, and a
    // re-run that resumes the place takes the request on.
    let started = Instant::now();
    let mut hinted = false;
    loop {
        let jobs = queue.snapshot()?.jobs;
        let left: Vec<&Job> = jobs
            .iter()
            .filter(|job| stopping.iter().any(|(stopped, _)| stopped.id == job.id))
            .collect();
        if left.is_empty() {
            break;
        }
        for job in &left {
            if job.state == JobState::Parked || job.is_orphaned() {
                queue.stop(job.id, args.kill)?;
            } else if let Some(owner) = job.owner.filter(ProcessId::is_stopped)
                && let Err(source) = owner.signal(libc::SIGCONT)
            {
                // Only if it still holds the job, stopped: since the
                // snapshot it may have parked the job, handed it over or
                // died, and the next look then stops the job without it.
                let jobs = queue.snapshot()?.jobs;
                if owner.is_stopped()
                    && jobs
                        .iter()
                        .any(|j| j.id == job.id && j.owner == Some(owner))
                {
                    return Err(Error::CannotSignal {
                        id: job.id,
                        why: format!(
                            "its loadmaster (process {}) is stopped, and could not be continued \
                             to act on the request",
                            owner.pid
                        ),
                        source,
                    });
                }
            }
        }
        if !hinted && started.elapsed() >= CANCEL_HINT_AFTER {
            hinted = true;
            for job in left {
                say!(
                    "loadmaster: #{} `{}` is still stopping{}",
                    job.id,
                    job.task,
                    if args.kill {
                        String::new()
                    } else {
                        format!("; `loadmaster cancel --kill {}` ends it at once", job.id)
                    }
                );
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
    for (job, what) in &stopping {
        out += &format!("canceled #{} `{}` ({what})\n", job.id, job.task);
    }
    print(&out);
    Ok(0)
}
