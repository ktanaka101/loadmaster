//! `loadmaster check` validates the configs and says what they mean on this
//! machine, without touching the queue: usable in CI.

mod common;

use common::*;

#[test]
fn a_valid_config_passes_with_its_warnings() {
    let sandbox = Sandbox::new(
        4,
        &(task("lint", 1, &["true"]) + &task("e2e", 8, &["true"])),
    );
    // In CI there is no queue yet; checking must not create one.
    std::fs::remove_dir(&sandbox.state).unwrap();
    let output = sandbox.run(&["check"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let out = stdout(&output);
    // The repository's path as the OS resolves the working directory.
    let config = std::fs::canonicalize(&sandbox.repo)
        .unwrap()
        .join(".loadmaster.toml");
    assert!(
        out.starts_with(&format!(
            "{}: valid\ncapacity: 4 (set in {})\n",
            config.display(),
            sandbox.machine_config.display()
        )),
        "{out}"
    );
    assert!(out.contains("\ne2e   8 (counts as 4)  -     0\n"), "{out}");
    assert!(
        out.contains("warning: `e2e` has weight 8, above this machine's capacity of 4"),
        "{out}"
    );
    assert!(!sandbox.state.exists(), "check created the state directory");
}

#[test]
fn an_invalid_config_fails() {
    let sandbox = Sandbox::new(4, "[tasks.lint]\nwieght = 1\ncmd = [\"true\"]\n");
    let output = sandbox.run(&["check"]);
    assert_eq!(output.status.code(), Some(125));
    let err = stderr(&output);
    assert!(err.contains("unknown field `wieght`"), "{err}");
    assert!(
        err.contains("an unknown key is a typo, or one that a newer version added"),
        "{err}"
    );
}

#[test]
fn a_config_for_a_newer_version_says_so() {
    let sandbox = Sandbox::new(
        4,
        "min_version = \"99.0\"\n[tasks.lint]\nweight = 1\nnew_key = true\ncmd = [\"true\"]\n",
    );
    for args in [&["check"][..], &["run", "lint"]] {
        let output = sandbox.run(args);
        assert_eq!(output.status.code(), Some(125), "{args:?}");
        let err = stderr(&output);
        assert!(
            err.contains("needs loadmaster 99.0 or newer (its `min_version`), but this is"),
            "{args:?}: {err}"
        );
    }
}

#[test]
fn a_broken_machine_config_fails() {
    let sandbox = Sandbox::new(4, &task("lint", 1, &["true"]));
    std::fs::write(&sandbox.machine_config, "capasity = 4\n").unwrap();
    let output = sandbox.run(&["check"]);
    assert_eq!(output.status.code(), Some(125));
    assert!(stderr(&output).contains("unknown field `capasity`"));
}
