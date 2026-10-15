//! `loadmaster status --json`: the queue for tools, in a documented format
//! (README, "Status as JSON") whose changes are listed as breaking ones. The
//! table `status` prints for people may change in any release.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use loadmaster::config::MachineConfig;
use loadmaster::process;
use loadmaster::queue::{
    Admission, Cause, Job, JobId, JobRef, JobState, Need, OtherQueue, Outside, Shortage, Snapshot,
    Wait,
};

/// The version of the format, in its `format` field.
const FORMAT: u32 = 1;

/// What `status --json` prints.
pub struct Status<'a> {
    pub machine: &'a MachineConfig,
    pub snapshot: &'a Snapshot,
    /// The CPU busy beyond what running jobs got, if jobs adapt to it.
    pub outside: Option<Outside>,
    /// Each machine-wide resource in use: name, used, capacity.
    pub resources: &'a [(&'a str, u64, Option<u32>)],
    pub others: &'a [OtherQueue],
    /// The admission rule's decision for every job not running.
    pub explained: &'a [(JobId, Admission)],
}

pub fn status(status: &Status) -> Value {
    let machine = status.machine;
    let jobs: Vec<Value> = status
        .snapshot
        .jobs
        .iter()
        .map(|job| {
            let admission = status
                .explained
                .iter()
                .find(|(id, _)| *id == job.id)
                .map(|(_, admission)| admission);
            job_value(job, admission)
        })
        .collect();
    json!({
        "format": FORMAT,
        "capacity": machine.capacity,
        "capacity_source": machine.capacity_source.to_string(),
        "measure_only": machine.measure_only,
        "cores_in_use": status.snapshot.used(),
        "cpu": status.outside.map(|outside| json!({
            "cores": outside.cores,
            "busy_beyond_running_jobs": (outside.busy * 10.0).round() / 10.0,
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
            "running": other.running,
        })).collect::<Vec<_>>(),
        "reclaimed": status.snapshot.reaped.len(),
        "jobs": jobs,
    })
}

fn unix_ms(t: SystemTime) -> Option<u64> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn job_value(job: &Job, admission: Option<&Admission>) -> Value {
    let (state, _) = super::shown_state(job);
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
        "enqueued_at_ms": unix_ms(job.enqueued_at),
        "started_at_ms": job.started_at.and_then(unix_ms),
        "parked_at_ms": job.parked_at.and_then(unix_ms),
        "capacity": job.capacity,
        "adapts_to_outside_cpu": job.adapt_cpu,
        "measure_only": job.measure_only,
        "processes": processes,
        "wait": admission.map(|admission| match admission {
            Admission::Admitted => json!({ "starts_now": true, "ahead": null, "for": [], "text": "about to start" }),
            Admission::Waiting(wait) => wait_value(wait),
        }),
    })
}

fn wait_value(wait: &Wait) -> Value {
    json!({
        "starts_now": false,
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
