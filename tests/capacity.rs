//! Acceptance criteria 1 and 7: the weight running at once never exceeds the
//! capacity, verified from the tasks' own records rather than loadmaster's.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use common::*;

const CAPACITY: u32 = 8;
const WEIGHTS: [u32; 5] = [1, 2, 3, 4, 8];
const JOBS: usize = 50;

#[test]
fn concurrent_weight_never_exceeds_capacity() {
    let events = tempfile::NamedTempFile::new().unwrap();
    let log = q(events.path());
    // Each task appends a start and an end event from inside the child. Appends
    // are atomic and ordered, so the file is a faithful timeline of execution.
    let tasks: String = WEIGHTS
        .iter()
        .map(|w| {
            sh_task(
                &format!("w{w}"),
                *w,
                &format!(
                    "echo \"start $$ $LOADMASTER_WEIGHT\" >> {log}; sleep 0.1; \
                     echo \"end $$ $LOADMASTER_WEIGHT\" >> {log}"
                ),
            )
        })
        .collect();
    let sandbox = Arc::new(Sandbox::new(CAPACITY, &tasks));

    // Also watch loadmaster's own bookkeeping while the storm runs.
    let done = Arc::new(AtomicBool::new(false));
    let max_used = Arc::new(AtomicU64::new(0));
    let sampler = {
        let (sandbox, done, max_used) = (sandbox.clone(), done.clone(), max_used.clone());
        thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                max_used.fetch_max(sandbox.snapshot().used(), Ordering::SeqCst);
                thread::sleep(Duration::from_millis(5));
            }
        })
    };

    let mut children: Vec<_> = (0..JOBS)
        .map(|i| sandbox.spawn(&["run", &format!("w{}", WEIGHTS[i % WEIGHTS.len()])]))
        .collect();
    for child in &mut children {
        assert!(wait(child).success());
    }
    done.store(true, Ordering::SeqCst);
    sampler.join().unwrap();

    let timeline = std::fs::read_to_string(events.path()).unwrap();
    let (mut running, mut jobs, mut peak_jobs, mut starts) = (0u32, 0usize, 0usize, 0usize);
    for line in timeline.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let weight: u32 = fields[2].parse().unwrap();
        match fields[0] {
            "start" => {
                running += weight;
                jobs += 1;
                starts += 1;
            }
            "end" => {
                running -= weight;
                jobs -= 1;
            }
            other => panic!("unexpected event {other:?}"),
        }
        assert!(
            running <= CAPACITY,
            "{running} weight ran at once, capacity is {CAPACITY}\n{timeline}"
        );
        peak_jobs = peak_jobs.max(jobs);
    }
    assert_eq!(starts, JOBS, "every job ran exactly once");
    assert_eq!(running, 0);
    // The jobs really overlapped: admission is not merely serializing them.
    assert!(peak_jobs >= 2, "jobs never ran concurrently");
    let max_used = max_used.load(Ordering::SeqCst);
    assert!(
        max_used <= u64::from(CAPACITY),
        "queue recorded {max_used} in use"
    );
    assert!(sandbox.snapshot().jobs.is_empty());
}

#[test]
fn a_weight_above_the_capacity_runs_alone() {
    // Weights are committed with the repository, the capacity belongs to the
    // machine: on a smaller machine the task still runs, taking all of it.
    let sandbox = Sandbox::new(4, "");
    let tasks = sh_task("hold", 1, &sandbox.hold_script("hold"))
        + &sh_task("big", 8, "echo \"weight=$LOADMASTER_WEIGHT\"");
    std::fs::write(sandbox.repo.join(".loadmaster.toml"), tasks).unwrap();
    let mut holder = sandbox.spawn(&["run", "hold"]);
    wait_until("the task to start", || sandbox.started("hold"));

    let mut big = sandbox
        .cmd(&["run", "big"])
        .stdout(std::fs::File::create(sandbox.path("big.out")).unwrap())
        .stderr(std::fs::File::create(sandbox.path("big.err")).unwrap())
        .spawn()
        .unwrap();
    wait_until("big to wait for the whole capacity", || {
        sandbox
            .job("big")
            .is_some_and(|j| j.state == loadmaster::queue::JobState::Waiting)
    });
    assert_eq!(sandbox.job("big").unwrap().weight, 4);

    sandbox.release("hold");
    assert!(wait(&mut holder).success());
    assert!(wait(&mut big).success(), "{}", sandbox.read("big.err"));
    assert_eq!(sandbox.read("big.out"), "weight=4\n");
    let err = sandbox.read("big.err");
    assert!(
        err.contains("`big` has weight 8, above this machine's capacity of 4"),
        "{err}"
    );
}
