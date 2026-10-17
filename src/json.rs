//! `loadmaster status --json`: the queue for tools, in a documented format
//! (README, "Status as JSON"). Fields may be added; any other change is a new
//! format, listed as a breaking change. The table `status` prints for people
//! may change in any release.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use loadmaster::config::MachineConfig;
use loadmaster::process;
use loadmaster::queue::{
    Admission, Cause, Job, JobId, JobRef, JobState, Need, OtherQueue, Outside, Shortage, Snapshot,
    Use, Wait,
};
use loadmaster::report::Check;
use loadmaster::usual::Usual;

/// The version of the format, in its `format` field.
const FORMAT: u32 = 2;

/// What `status --json` prints.
pub struct Status<'a> {
    pub machine: &'a MachineConfig,
    pub snapshot: &'a Snapshot,
    /// The CPU busy beyond what running jobs got, if it was measured: the
    /// rule's input for jobs that adapt to it.
    pub outside: Option<Outside>,
    /// Each machine-wide resource in use: name, used, capacity.
    pub resources: &'a [(&'a str, u64, Option<u32>)],
    pub others: &'a [OtherQueue],
    /// The admission rule's decision for every job not running, with the
    /// cores a job that may start starts with.
    pub explained: &'a [(JobId, Admission, Option<u32>)],
    /// The jobs of 0.12 to 0.14 left in a queue that is moving to this
    /// version's admission protocol, if it is.
    pub moving: Option<usize>,
    /// The checkout `status` was run from, if any.
    pub here: Option<&'a Path>,
    /// How long jobs usually run.
    pub usual: &'a Usual,
    /// The checks of `here` that ended lately, latest first.
    pub recent: &'a [Check],
}

pub fn status(status: &Status) -> Value {
    let machine = status.machine;
    let jobs: Vec<Value> = status
        .snapshot
        .jobs
        .iter()
        .map(|job| {
            let decision = status
                .explained
                .iter()
                .find(|(id, _, _)| *id == job.id)
                .map(|(_, admission, cores)| (admission, *cores));
            let mut value = job_value(job, decision);
            value["same_checkout"] = json!(status.here == Some(job.checkout.as_path()));
            value["usual_run"] = status.usual.of_job(job).map_or(Value::Null, |usual| {
                json!({
                    "median_ms": u64::try_from(usual.median.as_millis()).unwrap_or(u64::MAX),
                    "runs": usual.runs,
                    "latest_at_ms": usual.latest_at,
                })
            });
            value
        })
        .collect();
    json!({
        "format": FORMAT,
        "protocol": status.snapshot.protocol,
        "moving": status.moving.map(|left| json!({ "left": left })),
        "capacity": machine.capacity,
        "capacity_source": machine.capacity_source.to_string(),
        "measure_only": machine.measure_only,
        "cores_in_use": status.snapshot.used(),
        "cpu": status.outside.map(|outside| json!({
            "cores": outside.cores,
            "busy_beyond_running_jobs": outside.busy,
            "start_limit": outside.cpu_limit(machine.capacity),
        })),
        "load_average": process::load_average(),
        "resources": status.resources.iter().map(|&(name, used, capacity)| json!({
            "name": name,
            "in_use": used,
            "capacity": capacity,
        })).collect::<Vec<_>>(),
        "other_queues": status.others.iter().map(|other| json!({
            "file": other.file,
            "readable": other.jobs.is_some(),
            "running": other.jobs.as_ref().map(|jobs| &jobs.running),
            "waiting": other.jobs.as_ref().map(|jobs| &jobs.waiting),
            "parked": other.jobs.as_ref().map(|jobs| &jobs.parked),
        })).collect::<Vec<_>>(),
        "reclaimed": status.snapshot.reaped.len(),
        "jobs": jobs,
        "recent": status.recent.iter().map(|check| json!({
            "id": check.job,
            "task": check.task,
            "result": crate::cli::status::result_code(check.result),
            "exit_code": check.exit_code,
            "run_ms": check.run_ms,
            "wait_ms": check.wait_ms,
            "ended_at_ms": check.end(),
        })).collect::<Vec<_>>(),
    })
}

/// The queue a status of this format describes, as the admission rule sees
/// it, for `check --scenario`: its jobs in line order and the CPU busy
/// outside the queue (`cpu`, or none). Only what the rule reads is needed of
/// each job: `id`, `state`, `task`, `weight`, `cores` (of a running one),
/// `uses`, `checkout`, `capacity`, `adapts_to_outside_cpu` and
/// `measure_only`; anything else is ignored. Its states are as they are: no
/// process is looked at, no place expires.
pub fn scenario(value: &Value) -> Result<(Vec<Job>, Option<Outside>), String> {
    if value["format"] != json!(FORMAT) {
        return Err(format!(
            "it is not a status of format {FORMAT} (`format` is {})",
            value["format"]
        ));
    }
    let outside = match &value["cpu"] {
        Value::Null => None,
        cpu => Some(Outside {
            busy: cpu["busy_beyond_running_jobs"]
                .as_f64()
                .ok_or("`cpu` has no number `busy_beyond_running_jobs`")?,
            cores: number(&cpu["cores"]).ok_or("`cpu` has no number `cores`")?,
        }),
    };
    let mut jobs = value["jobs"]
        .as_array()
        .ok_or("it has no list `jobs`")?
        .iter()
        .map(scenario_job)
        .collect::<Result<Vec<Job>, String>>()?;
    jobs.sort_by_key(|job| job.id);
    if let Some(pair) = jobs.windows(2).find(|pair| pair[0].id == pair[1].id) {
        return Err(format!("two jobs have the ID {}", pair[0].id));
    }
    Ok((jobs, outside))
}

fn number(value: &Value) -> Option<u32> {
    u32::try_from(value.as_u64()?).ok()
}

fn scenario_job(value: &Value) -> Result<Job, String> {
    let id: JobId = value["id"].as_i64().ok_or("a job has no number `id`")?;
    let missing = |field: &str| format!("job #{id} has no valid `{field}`");
    let get = |field: &str| number(&value[field]).ok_or_else(|| missing(field));
    let flag = |field: &str| value[field].as_bool().ok_or_else(|| missing(field));
    let state = match value["state"].as_str() {
        Some("running" | "orphaned" | "lingering") => JobState::Running,
        Some("waiting") => JobState::Waiting,
        Some("parked") => JobState::Parked,
        _ => return Err(missing("state")),
    };
    let uses = value["uses"]
        .as_object()
        .ok_or_else(|| missing("uses"))?
        .iter()
        .map(|(name, u)| {
            let limit = |field: &str| match &u[field] {
                Value::Null => Ok(None),
                v => number(v).map(Some).ok_or_else(|| missing("uses")),
            };
            Ok(Use {
                name: name.clone(),
                amount: number(&u["amount"]).ok_or_else(|| missing("uses"))?,
                capacity: limit("capacity")?,
                per_checkout: limit("per_checkout")?,
            })
        })
        .collect::<Result<Vec<Use>, String>>()?;
    let checkout = PathBuf::from(
        value["checkout"]
            .as_str()
            .ok_or_else(|| missing("checkout"))?,
    );
    Ok(Job {
        id,
        task: value["task"]
            .as_str()
            .ok_or_else(|| missing("task"))?
            .to_owned(),
        weight: get("weight")?,
        state,
        dir: checkout.clone(),
        args: Vec::new(),
        command: String::new(),
        owner: None,
        child: None,
        enqueued_at: UNIX_EPOCH,
        parked_at: None,
        started_at: None,
        capacity: get("capacity")?,
        measure_only: flag("measure_only")?,
        uses,
        checkout,
        adapt_cpu: flag("adapts_to_outside_cpu")?,
        stop_signal: libc::SIGTERM,
        granted: match state {
            JobState::Running => Some(get("cores")?),
            _ => None,
        },
        state_since: None,
        // Without one, as a job of 0.12 to 0.14 recorded it.
        min_weight: match &value["min_weight"] {
            Value::Null => None,
            v => Some(number(v).ok_or_else(|| missing("min_weight"))?),
        },
    })
    .and_then(|job| match job.min_weight {
        // As in a config; above the capacity, like a weight, it counts as
        // all of it.
        Some(least) if !(1..=job.weight).contains(&least) => Err(format!(
            "job #{id} has min_weight {least}, which is not from 1 to its weight {}",
            job.weight
        )),
        _ => Ok(job),
    })
}

fn unix_ms(t: SystemTime) -> Option<u64> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn job_value(job: &Job, decision: Option<(&Admission, Option<u32>)>) -> Value {
    let (state, since) = crate::cli::status::shown_state(job);
    let processes: Option<Vec<Value>> =
        (state == "lingering")
            .then_some(job.child)
            .flatten()
            .map(|group| {
                group
                    .members()
                    .iter()
                    .map(|p| json!({ "pid": p.pid, "name": p.name }))
                    .collect()
            });
    json!({
        "id": job.id,
        "state": state,
        "task": job.task,
        "weight": job.weight,
        "min_weight": job.least(),
        "cores": job.granted.filter(|_| job.state == JobState::Running),
        "uses": job.uses.iter().map(|u| (u.name.clone(), json!({
            "amount": u.amount,
            "capacity": u.capacity,
            "per_checkout": u.per_checkout,
        }))).collect::<serde_json::Map<_, _>>(),
        // Paths that are not UTF-8 have their bytes replaced.
        "dir": job.dir.to_string_lossy(),
        "checkout": job.checkout.to_string_lossy(),
        "args": job.args,
        "pid": job.owner.map(|o| o.pid),
        "pgid": job.child.map(|c| c.pgid),
        "state_since_ms": since.and_then(unix_ms),
        "enqueued_at_ms": unix_ms(job.enqueued_at),
        "started_at_ms": job.started_at.and_then(unix_ms),
        "parked_at_ms": job.parked_at.and_then(unix_ms),
        "capacity": job.capacity,
        "adapts_to_outside_cpu": job.adapt_cpu,
        "measure_only": job.measure_only,
        "processes": processes,
        "wait": decision.map(|(admission, cores)| match admission {
            Admission::Admitted => json!({
                "starts_now": true,
                "cores": cores,
                "ahead": null,
                "for": [],
                "text": "about to start",
            }),
            Admission::Waiting(wait) => wait_value(wait),
        }),
    })
}

fn wait_value(wait: &Wait) -> Value {
    json!({
        "starts_now": false,
        "cores": null,
        "ahead": wait.ahead,
        "for": wait.short.iter().map(shortage_value).collect::<Vec<_>>(),
        "text": wait.waits_for(),
    })
}

fn ids(jobs: &[JobRef]) -> Vec<JobId> {
    jobs.iter().map(|job| job.id).collect()
}

fn shortage_value(shortage: &Shortage) -> Value {
    let (need, resource) = match &shortage.need {
        Need::Cores => ("cores", None),
        Need::Resource(name) => ("resource", Some(name)),
        Need::CheckoutResource(name) => ("checkout_resource", Some(name)),
    };
    let mut value = json!({
        "need": need,
        "resource": resource,
        "amount": shortage.amount,
    });
    let cause = match &shortage.cause {
        Cause::InUse {
            used,
            limit,
            by,
            limit_by,
            outside,
        } => json!({
            "reason": "in_use",
            "used": used,
            "limit": limit,
            "limit_set_by": limit_by.as_ref().map(|job| job.id),
            "held_by": ids(by),
            "cpu_outside": outside.map(|o| json!({ "busy": o.busy, "cores": o.cores })),
        }),
        Cause::BesideStart {
            start,
            used,
            limit,
            beside,
            limit_by,
        } => json!({
            "reason": "beside_start",
            "used": used,
            "limit": limit,
            "limit_set_by": limit_by.as_ref().map(|job| job.id),
            "start": start.id,
            "beside": beside.as_deref().map(ids),
        }),
    };
    if let (Value::Object(value), Value::Object(cause)) = (&mut value, cause) {
        value.extend(cause);
    }
    value
}
