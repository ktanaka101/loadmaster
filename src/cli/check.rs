//! `loadmaster check`, and `check --scenario`.

use crate::*;

pub(crate) fn check() -> Result<i32> {
    let cwd = env::current_dir().map_err(|e| Error::io("cannot read the current directory", e))?;
    let repo = RepoConfig::discover(&cwd)?;
    let machine = MachineConfig::load()?;
    print(&check::render(&repo, &machine));
    Ok(0)
}

/// `check --scenario`: what the admission rule makes of the queue a file
/// describes, job by job in line order.
pub(crate) fn check_scenario(path: &Path) -> Result<i32> {
    let text = if path == Path::new("-") {
        io::read_to_string(io::stdin()).map_err(|e| Error::io("cannot read stdin", e))?
    } else {
        fs::read_to_string(path)
            .map_err(|e| Error::io(format!("cannot read {}", path.display()), e))?
    };
    let invalid =
        |message: String| Error::Usage(format!("{} is not a scenario: {message}", path.display()));
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| invalid(e.to_string()))?;
    let (jobs, outside) = json::scenario(&value).map_err(invalid)?;
    let mut out = match outside {
        Some(o) => format!(
            "CPU busy beyond what running jobs got: {} of {} cores\n",
            o.busy, o.cores
        ),
        None => "no CPU busy outside the queue given\n".to_owned(),
    };
    if value["protocol"] != serde_json::json!(2) {
        out += "judged by the admission protocol of 0.15 and later: a job of 0.12 to 0.14 \
                (without `min_weight`) may start with as few as one core if it adapts to CPU \
                outside the queue, else only with its weight\n";
    }
    let explained = queue::explain_with_cores(&jobs, outside);
    for job in &jobs {
        let who = format!("#{} `{}`", job.id, job.task);
        let cores = |n: u32| format!("{n} core{}", if n == 1 { "" } else { "s" });
        let decision = explained.iter().find(|(id, _, _)| *id == job.id);
        let start = decision
            .and_then(|(_, _, cores)| *cores)
            .or(job.granted)
            .unwrap_or(job.most());
        let decision = decision.map(|(_, admission, _)| admission);
        out += &match (job.state, decision) {
            (JobState::Running, _) => format!("{who} holds {}\n", cores(start)),
            // It starts at once, whatever the line and the capacity.
            (JobState::Waiting, _) if job.measure_only => format!(
                "{who} is measure-only: it starts at once with {}\n",
                cores(job.most())
            ),
            (JobState::Parked, _) if job.measure_only => format!(
                "{who} is parked and measure-only: resumed, it starts at once with {}\n",
                cores(job.most())
            ),
            (JobState::Parked, Some(Admission::Admitted)) => {
                format!(
                    "{who} is parked; resumed now, it would start with {}\n",
                    cores(start)
                )
            }
            (JobState::Parked, Some(Admission::Waiting(wait))) => format!(
                "{who} is parked; resumed now, it would wait for {}\n",
                wait.waits_for()
            ),
            (_, Some(Admission::Admitted)) => {
                format!("{who} starts now with {}\n", cores(start))
            }
            (_, Some(Admission::Waiting(wait))) => {
                format!("{who} waits for {}\n", wait.waits_for())
            }
            (_, None) => String::new(),
        };
    }
    print(&out);
    Ok(0)
}
