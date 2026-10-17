//! `loadmaster doctor` says whether loadmaster works for agents here, and
//! what to change where it does not.

mod common;

use std::os::unix::fs::PermissionsExt;

use common::*;

fn doctor(sandbox: &Sandbox, home: &std::path::Path, args: &[&str]) -> std::process::Output {
    let mut cmd = sandbox.cmd(&[&["doctor"], args].concat());
    cmd.env("HOME", home).env_remove("CODEX_HOME");
    common::output(cmd)
}

#[test]
fn doctor_says_what_works_and_what_to_change() {
    let sandbox = Sandbox::new(4, &sh_task("lint", 1, "true"));
    let home = sandbox.path("home");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(
        home.join(".claude/settings.json"),
        r#"{"sandbox": {"enabled": true}, "env": {"LOADMASTER_MAX_WAIT": "60"}}"#,
    )
    .unwrap();
    let output = doctor(&sandbox, &home, &[]);
    let out = stdout(&output);
    // Notes are not problems.
    assert_eq!(output.status.code(), Some(0), "{out}");
    assert!(
        out.contains("ok: state directory: this process may write to "),
        "{out}"
    );
    assert!(out.contains("ok: .loadmaster.toml: "), "{out}");
    assert!(
        out.contains("note: Claude Code: its sandbox is on ("),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "  fix: add to {}: {{\"sandbox\": {{\"filesystem\": {{\"allowWrite\": [\"{}\"]}}}}}}",
            home.join(".claude/settings.json").display(),
            sandbox.state.display()
        )),
        "{out}"
    );
    assert!(
        out.contains("note: instructions: no AGENTS.md or CLAUDE.md"),
        "{out}"
    );
    // Problems first.
    let levels: Vec<&str> = out
        .lines()
        .filter_map(|l| l.split(':').next())
        .filter(|w| ["problem", "note", "unknown", "ok"].contains(w))
        .collect();
    let mut sorted = levels.clone();
    sorted.sort_by_key(|w| {
        ["problem", "note", "unknown", "ok"]
            .iter()
            .position(|x| x == w)
    });
    assert_eq!(levels, sorted, "{out}");

    // An instruction file that tells of loadmaster, and the list allowing
    // the state directory: no notes of them.
    std::fs::write(
        sandbox.repo.join("AGENTS.md"),
        "Run `loadmaster guide` first.",
    )
    .unwrap();
    std::fs::write(
        home.join(".claude/settings.json"),
        format!(
            r#"{{"sandbox": {{"enabled": true, "filesystem": {{"allowWrite": ["{}"]}}}},
                "env": {{"LOADMASTER_MAX_WAIT": "60"}}}}"#,
            sandbox.state.display()
        ),
    )
    .unwrap();
    let json: serde_json::Value =
        serde_json::from_slice(&doctor(&sandbox, &home, &["--json"]).stdout).unwrap();
    assert_eq!(json["problems"], 0);
    let findings = json["findings"].as_array().unwrap();
    let of = |check: &str| -> Vec<&str> {
        findings
            .iter()
            .filter(|f| f["check"] == check)
            .map(|f| f["level"].as_str().unwrap())
            .collect()
    };
    assert_eq!(of("instructions"), ["ok"]);
    assert_eq!(of("Claude Code"), ["ok", "ok"]);
}

#[test]
fn a_state_directory_it_may_not_write_is_a_problem() {
    // Permissions do not stop root.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let sandbox = Sandbox::new(4, &sh_task("lint", 1, "true"));
    let home = sandbox.path("home");
    std::fs::create_dir_all(&home).unwrap();
    let locked = sandbox.path("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    let mut cmd = sandbox.cmd(&["doctor"]);
    cmd.env("HOME", &home)
        .env("LOADMASTER_STATE_DIR", &locked)
        .env_remove("CODEX_HOME");
    let output = common::output(cmd);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(1), "{out}");
    let first = out
        .lines()
        .find(|l| !l.is_empty() && !l.starts_with("loadmaster doctor"))
        .unwrap();
    assert!(
        first.starts_with("problem: state directory: this process may not write to "),
        "{out}"
    );
    assert!(out.contains("  fix: allow writing to "), "{out}");
}

#[test]
fn a_repository_whose_scripts_go_through_loadmaster_needs_no_instruction() {
    let sandbox = Sandbox::new(4, &sh_task("test", 1, "true"));
    let home = sandbox.path("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        sandbox.repo.join("package.json"),
        r#"{"scripts": {"test": "loadmaster run test"}}"#,
    )
    .unwrap();
    let json: serde_json::Value =
        serde_json::from_slice(&doctor(&sandbox, &home, &["--json"]).stdout).unwrap();
    let found = json["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["check"] == "instructions")
        .unwrap()
        .clone();
    assert_eq!(found["level"], "ok", "{json:#}");
    assert!(
        found["message"]
            .as_str()
            .unwrap()
            .contains("package.json runs checks through loadmaster"),
        "{json:#}"
    );
    assert_eq!(found["fix"], serde_json::Value::Null);
}
