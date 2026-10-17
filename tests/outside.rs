//! Jobs start with fewer cores while CPU is busy outside the queue. Its own
//! test binary: it keeps every core busy, which would slow other tests.

mod common;

use std::process::{Child, Command};
use std::time::Duration;

use common::*;

/// Processes that keep every core busy, outside the queue, until dropped.
struct Burners(Vec<Child>);

impl Burners {
    fn start(cores: u32) -> Burners {
        Burners(
            (0..cores)
                .map(|_| {
                    Command::new("sh")
                        .args(["-c", "while :; do :; done"])
                        .spawn()
                        .unwrap()
                })
                .collect(),
        )
    }
}

impl Drop for Burners {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn jobs_start_with_fewer_cores_while_cpu_is_busy_outside_the_queue() {
    let cores = loadmaster::process::online_cpus().unwrap() as u32;
    // The machine must be one this test can keep busy, with a core to spare
    // below the weight.
    let usable = std::thread::available_parallelism().unwrap().get() as u32;
    if cores < 2 || usable < cores {
        eprintln!("skipped: {usable} of {cores} CPUs usable here");
        return;
    }
    // `show` may start with as few as one core, `fixed` only with its weight.
    let tasks = sh_task("show", cores, "echo $LOADMASTER_WEIGHT")
        .replacen("cmd = ", "min_weight = 1\ncmd = ", 1)
        + &sh_task("fixed", cores, "echo $LOADMASTER_WEIGHT")
        // Long enough for the machine's CPU time to be measured.
        + &sh_task("fail", 1, "sleep 0.5; exit 1");
    let sandbox = Sandbox::new(cores, &tasks);
    // The default: adapt.
    std::fs::write(&sandbox.machine_config, format!("capacity = {cores}\n")).unwrap();
    // Twice over, so the CPUs stay saturated whatever the scheduler does.
    let burners = Burners::start(2 * cores);
    std::thread::sleep(Duration::from_millis(500));

    let output = sandbox.run(&["run", "show"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let granted: u32 = stdout(&output).trim().parse().unwrap();
    assert!(granted < cores, "LOADMASTER_WEIGHT={granted} of {cores}");
    assert!(
        stderr(&output).contains(&format!(
            "`show` started with {granted} of its {cores} cores, the most that fit now"
        )),
        "{}",
        stderr(&output)
    );
    // Nothing runs ahead of it: without its consent to fewer, it starts with
    // all of them all the same.
    let output = sandbox.run(&["run", "fixed"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), cores.to_string());
    let status = stdout(&sandbox.run(&["status"]));
    assert!(
        status.contains("CPU busy beyond what running jobs got: "),
        "{status}"
    );
    assert!(status.contains(", so jobs start with at most "), "{status}");

    // A failure while the CPUs are all busy says so.
    let output = sandbox.run(&["run", "fail"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("loadmaster: note: the machine's CPUs were "),
        "{}",
        stderr(&output)
    );
    drop(burners);

    let history = sandbox.read("state/history.jsonl");
    let records: Vec<serde_json::Value> = history
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let of = |task: &str| records.iter().find(|r| r["task"] == task).unwrap();
    assert_eq!(of("show")["granted"], granted);
    assert_eq!(of("show")["min_weight"], 1);
    assert_eq!(of("fixed")["granted"], serde_json::Value::Null);
    // `show` may end before a clock tick: `fail` runs long enough.
    let busy = of("fail")["machine_busy"].as_f64().unwrap();
    assert!(busy >= 0.95 * f64::from(cores), "{}", of("fail"));
}
