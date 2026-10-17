//! `loadmaster doctor`: whether loadmaster works for agents here, and what
//! to change where it does not (see [`loadmaster::doctor`]). It changes no
//! settings and no jobs: it creates and removes a file in the state
//! directory, to see whether this process may write there (run by an
//! agent's tool, the sure test of its sandbox), and opens this queue's file
//! read-only.

use crate::cli::run::max_wait_from_env;
use crate::*;
use loadmaster::doctor::{self, Finding, Level, Settings};

pub(crate) fn doctor(args: DoctorArgs) -> Result<i32> {
    let mut findings = Vec::new();
    let home = env::var_os("HOME").map(PathBuf::from);
    findings.push(binary());
    let cwd = env::current_dir().map_err(|e| Error::io("cannot read the current directory", e))?;
    let repo = repo(&cwd, &mut findings);
    findings.push(match MachineConfig::load() {
        Ok(machine) => ok(
            "machine config",
            format!(
                "capacity {} cores ({})",
                machine.capacity, machine.capacity_source
            ),
        ),
        Err(e) => problem("machine config", e.to_string(), e.next()),
    });
    let max_wait = match max_wait_from_env() {
        Ok(max_wait) => max_wait,
        Err(e) => {
            findings.push(problem("LOADMASTER_MAX_WAIT", e.to_string(), e.next()));
            None
        }
    };
    match config::state_dir() {
        Ok(dir) => {
            findings.push(writable(&dir));
            findings.extend(queue_file(&dir));
            if let Some(others) = other_queues(&dir) {
                findings.push(note("other queues", others, None));
            }
            let root = repo.as_ref().map(|r| r.dir().to_owned());
            findings.extend(doctor::claude_code(
                &claude_settings(home.as_deref(), root.as_deref()),
                &dir,
                home.as_deref(),
                max_wait,
            ));
            let codex = codex_config(home.as_deref());
            findings.extend(doctor::codex(
                codex
                    .as_ref()
                    .map(|(path, text)| (path.as_path(), text.as_str())),
                &dir,
                home.as_deref(),
            ));
        }
        Err(e) => findings.push(problem("state directory", e.to_string(), e.next())),
    }
    if let Some(repo) = &repo {
        let files: Vec<(PathBuf, String)> = ["AGENTS.md", "CLAUDE.md"]
            .iter()
            .map(|name| repo.dir().join(name))
            .filter_map(|path| Some((path.clone(), fs::read_to_string(&path).ok()?)))
            .collect();
        findings.push(doctor::instructions(repo.dir(), &files));
    }
    findings.sort_by_key(|f| f.level);
    let problems = findings
        .iter()
        .filter(|f| f.level == Level::Problem)
        .count();
    if args.json {
        let value = serde_json::json!({
            "problems": problems,
            "findings": findings.iter().map(|f| serde_json::json!({
                "level": f.level.word(),
                "check": f.check,
                "message": f.message,
                "fix": f.fix,
            })).collect::<Vec<_>>(),
        });
        print(&format!("{value:#}\n"));
    } else {
        let mut out = "loadmaster doctor: whether loadmaster works for agents here. It changes \
                       no settings and no jobs: it creates and removes a file in the state \
                       directory, to see whether this process may write there, and reading the \
                       queue files of 0.11 and before may fold their logs into them, as every \
                       loadmaster that reads them does.\n\n"
            .to_owned();
        for finding in &findings {
            out += &format!(
                "{}: {}: {}\n",
                finding.level.word(),
                finding.check,
                finding.message
            );
            if let Some(fix) = &finding.fix {
                out += &format!("  fix: {fix}\n");
            }
        }
        print(&out);
    }
    Ok(if problems > 0 { 1 } else { 0 })
}

fn ok(check: &'static str, message: impl Into<String>) -> Finding {
    found(Level::Ok, check, message, None)
}

fn note(check: &'static str, message: impl Into<String>, fix: Option<String>) -> Finding {
    found(Level::Note, check, message, fix)
}

fn problem(check: &'static str, message: impl Into<String>, fix: Option<String>) -> Finding {
    found(Level::Problem, check, message, fix)
}

fn found(
    level: Level,
    check: &'static str,
    message: impl Into<String>,
    fix: Option<String>,
) -> Finding {
    Finding {
        level,
        check,
        message: message.into(),
        fix,
    }
}

/// This binary, and whether `loadmaster` on PATH is the same one.
fn binary() -> Finding {
    let version = env!("CARGO_PKG_VERSION");
    let this = env::current_exe()
        .ok()
        .and_then(|p| fs::canonicalize(p).ok());
    let on_path = env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|dir| dir.join("loadmaster"))
            .find(|candidate| candidate.is_file())
    });
    let resolved = on_path.as_ref().and_then(|p| fs::canonicalize(p).ok());
    match (&this, &on_path) {
        (Some(this), Some(found)) if resolved.as_ref() != Some(this) => note(
            "loadmaster",
            format!(
                "this is {version} at {}, but `loadmaster` on PATH is {}: agents run that one",
                this.display(),
                found.display()
            ),
            Some("give every agent the same loadmaster (one install on PATH)".to_owned()),
        ),
        (Some(this), _) => ok("loadmaster", format!("{version} at {}", this.display())),
        (None, _) => ok("loadmaster", version.to_owned()),
    }
}

/// The repository config here, if any.
fn repo(cwd: &Path, findings: &mut Vec<Finding>) -> Option<RepoConfig> {
    match RepoConfig::locate(cwd) {
        Err(e) => {
            findings.push(note(
                ".loadmaster.toml",
                e.to_string(),
                Some("`loadmaster guide` says how to set one up".to_owned()),
            ));
            None
        }
        Ok(path) => match RepoConfig::load(&path) {
            Ok(repo) => {
                let names: Vec<&str> = repo.tasks().map(|t| t.name.as_str()).collect();
                findings.push(ok(
                    ".loadmaster.toml",
                    format!("{}: tasks {}", tilde(&path), names.join(", ")),
                ));
                Some(repo)
            }
            Err(e) => {
                findings.push(problem(".loadmaster.toml", e.to_string(), e.next()));
                None
            }
        },
    }
}

/// Whether this process may write to the state directory, tried by
/// creating and removing a file there (the directory too, if missing).
fn writable(dir: &Path) -> Finding {
    const CHECK: &str = "state directory";
    // A name no other process uses, nor this one before.
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tried = DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .and_then(|()| {
            let probe = dir.join(format!(".doctor-{}-{nanos}", std::process::id()));
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&probe)
                .and_then(|_| fs::remove_file(&probe))
        });
    match tried {
        Ok(()) => ok(CHECK, format!("this process may write to {}", tilde(dir))),
        // What a sandbox does: its fix is in the tool's settings.
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
            ) =>
        {
            problem(
                CHECK,
                format!("this process may not write to {} ({e})", tilde(dir)),
                Error::StateDirNotWritable {
                    dir: dir.to_owned(),
                    detail: e.to_string(),
                }
                .next(),
            )
        }
        // Anything else (a full disk, say) is not the sandbox's.
        Err(e) => problem(
            CHECK,
            format!("writing a file in {} failed: {e}", tilde(dir)),
            None,
        ),
    }
}

/// The queue file in `dir`, as it is.
fn queue_file(dir: &Path) -> Option<Finding> {
    const CHECK: &str = "queue";
    let path = dir.join(QUEUE_DB_FILE);
    Some(match queue::inspect(&path) {
        Ok(None) => ok(CHECK, "none yet: the first run makes it"),
        Ok(Some(file)) if file.is_new() => ok(CHECK, "not set up yet: the first run does"),
        Ok(Some(file)) if file.is_current() => ok(
            CHECK,
            format!("{}: {} job(s) in it", tilde(&path), file.jobs),
        ),
        Ok(Some(file)) if file.is_older() => note(
            CHECK,
            format!(
                "{} keeps the admission protocol of loadmaster 0.12 to 0.14{}: the first \
                 `loadmaster run` of this version moves it, and from then on older agents' runs \
                 fail",
                tilde(&path),
                if file.closed { ", and is moving" } else { "" }
            ),
            Some("give every agent on this machine this version at once".to_owned()),
        ),
        Ok(Some(file)) => problem(
            CHECK,
            format!(
                "{} has layout {} and protocol {}, which this version does not know: a later \
                 loadmaster made it",
                tilde(&path),
                file.layout,
                file.protocol.map_or("none".to_owned(), |p| p.to_string())
            ),
            Some("upgrade loadmaster to the version the other agents use".to_owned()),
        ),
        Err(e) => found(
            Level::Unknown,
            CHECK,
            format!("{} could not be read: {e}", tilde(&path)),
            None,
        ),
    })
}

/// Claude Code's settings files, from the lowest precedence to the
/// highest: user, project, local, managed. Only those that exist.
fn claude_settings(home: Option<&Path>, root: Option<&Path>) -> Vec<Settings> {
    let managed = if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json")
    } else {
        PathBuf::from("/etc/claude-code/managed-settings.json")
    };
    let paths = [
        home.map(|h| h.join(".claude/settings.json")),
        root.map(|r| r.join(".claude/settings.json")),
        root.map(|r| r.join(".claude/settings.local.json")),
        Some(managed),
    ];
    paths
        .into_iter()
        .flatten()
        .filter_map(|path| {
            let text = fs::read_to_string(&path).ok()?;
            Some(Settings {
                value: serde_json::from_str(&text).ok(),
                path,
            })
        })
        .collect()
}

/// Codex's config, `$CODEX_HOME/config.toml` or `~/.codex/config.toml`.
fn codex_config(home: Option<&Path>) -> Option<(PathBuf, String)> {
    let dir = env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".codex")))?;
    let path = dir.join("config.toml");
    let text = fs::read_to_string(&path).ok()?;
    Some((path, text))
}
