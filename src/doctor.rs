//! `loadmaster doctor`: whether loadmaster works for the agents of this
//! machine, and what to change where it does not. Each finding is as sure as
//! what it rests on: what this process could or could not do is a problem;
//! what an agent tool's settings files suggest is a note, since settings
//! have layers (managed, project, local, command line) and defaults that a
//! file does not show.

use std::path::{Path, PathBuf};

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Something that does not work, as this process found.
    Problem,
    /// Something that may not work, from what the files say.
    Note,
    /// Something that could not be told.
    Unknown,
    Ok,
}

impl Level {
    pub fn word(self) -> &'static str {
        match self {
            Level::Problem => "problem",
            Level::Note => "note",
            Level::Unknown => "unknown",
            Level::Ok => "ok",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub level: Level,
    /// What was checked, in a word or two: `state directory`, `Claude Code`.
    pub check: &'static str,
    pub message: String,
    /// What to change, where there is one thing to change.
    pub fix: Option<String>,
}

impl Finding {
    fn new(level: Level, check: &'static str, message: impl Into<String>) -> Finding {
        Finding {
            level,
            check,
            message: message.into(),
            fix: None,
        }
    }

    fn fix(mut self, fix: impl Into<String>) -> Finding {
        self.fix = Some(fix.into());
        self
    }
}

/// Whether a path an agent tool lets commands write to covers `dir`: the
/// path itself, a parent of it, or a parent followed by `/**` or `/*`. `~`
/// is `home`.
fn covers(entry: &str, dir: &Path, home: Option<&Path>) -> bool {
    let entry = entry.trim_end_matches("/**").trim_end_matches("/*");
    let path = match (entry.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ if entry == "~" => match home {
            Some(home) => home.to_owned(),
            None => return false,
        },
        _ => PathBuf::from(entry),
    };
    dir.starts_with(&path)
}

/// `dir` as a tool's settings would name it: under home as `~/...`.
fn named(dir: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| dir.strip_prefix(home).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => dir.display().to_string(),
    }
}

/// A Claude Code settings file: where it is, and what it says (`None` if it
/// could not be read as JSON).
pub struct Settings {
    pub path: PathBuf,
    pub value: Option<Value>,
}

/// What Claude Code's settings files say of its sandbox and its timeouts,
/// for a state directory `state_dir` and `LOADMASTER_MAX_WAIT` `max_wait`
/// (seconds, if set). `files` are from the lowest precedence to the
/// highest: user, project, local, managed.
pub fn claude_code(
    files: &[Settings],
    state_dir: &Path,
    home: Option<&Path>,
    max_wait: Option<u64>,
) -> Vec<Finding> {
    const CHECK: &str = "Claude Code";
    let mut out = Vec::new();
    for file in files.iter().filter(|f| f.value.is_none()) {
        out.push(Finding::new(
            Level::Unknown,
            CHECK,
            format!(
                "{} is not valid JSON, so it was not read",
                file.path.display()
            ),
        ));
    }
    let values: Vec<(&Path, &Value)> = files
        .iter()
        .filter_map(|f| Some((f.path.as_path(), f.value.as_ref()?)))
        .collect();
    if values.is_empty() {
        return out;
    }
    // The highest file that says decides; the lists add up.
    let sandbox = values
        .iter()
        .rev()
        .find_map(|(path, v)| Some((*path, v["sandbox"]["enabled"].as_bool()?)));
    let allowed: Vec<&str> = values
        .iter()
        .filter_map(|(_, v)| v["sandbox"]["filesystem"]["allowWrite"].as_array())
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let user = files.first().map(|f| f.path.display().to_string());
    match sandbox {
        Some((path, true)) if !allowed.iter().any(|e| covers(e, state_dir, home)) => {
            out.push(
                Finding::new(
                    Level::Note,
                    CHECK,
                    format!(
                        "its sandbox is on ({}), and no `sandbox.filesystem.allowWrite` lists {}: \
                         unless another setting does, `loadmaster` fails there with exit code \
                         125; `loadmaster doctor` run by Claude Code itself tells for sure",
                        path.display(),
                        named(state_dir, home)
                    ),
                )
                .fix(format!(
                    "add to {}: {{\"sandbox\": {{\"filesystem\": {{\"allowWrite\": [\"{}\"]}}}}}}",
                    user.as_deref().unwrap_or("~/.claude/settings.json"),
                    named(state_dir, home)
                )),
            );
        }
        Some((_, true)) => out.push(Finding::new(
            Level::Ok,
            CHECK,
            "its sandbox lets commands write to the state directory",
        )),
        _ => {}
    }
    // Its Bash tool's timeouts include the wait in line.
    let env = |name: &str| {
        values
            .iter()
            .rev()
            .find_map(|(_, v)| match &v["env"][name] {
                Value::String(s) => s.trim().parse::<u64>().ok(),
                Value::Number(n) => n.as_u64(),
                _ => None,
            })
    };
    let timeout_ms = env("BASH_DEFAULT_TIMEOUT_MS").unwrap_or(120_000);
    let max_wait = max_wait.or_else(|| env("LOADMASTER_MAX_WAIT"));
    match max_wait {
        None => out.push(
            Finding::new(
                Level::Note,
                CHECK,
                format!(
                    "its Bash tool ends a command after {}s by default, the wait in line \
                     included, and LOADMASTER_MAX_WAIT is not set: a check that waits that long \
                     is killed and loses its place",
                    timeout_ms / 1000
                ),
            )
            .fix(format!(
                "add to {}: {{\"env\": {{\"BASH_DEFAULT_TIMEOUT_MS\": \"1800000\", \
                 \"BASH_MAX_TIMEOUT_MS\": \"3600000\", \"LOADMASTER_MAX_WAIT\": \"600\"}}}}",
                user.as_deref().unwrap_or("~/.claude/settings.json")
            )),
        ),
        Some(secs) if secs * 1000 >= timeout_ms => out.push(
            Finding::new(
                Level::Note,
                CHECK,
                format!(
                    "LOADMASTER_MAX_WAIT is {secs}s, but its Bash tool ends a command after \
                     {}s by default, the wait in line included: loadmaster cannot give up first",
                    timeout_ms / 1000
                ),
            )
            .fix(
                "set LOADMASTER_MAX_WAIT well below BASH_DEFAULT_TIMEOUT_MS, leaving time for \
                 the run itself",
            ),
        ),
        Some(secs) => out.push(Finding::new(
            Level::Ok,
            CHECK,
            format!(
                "LOADMASTER_MAX_WAIT ({secs}s) is below its default timeout ({}s)",
                timeout_ms / 1000
            ),
        )),
    }
    out
}

/// What Codex's config (`~/.codex/config.toml`, if read) says of its
/// sandbox, for a state directory `state_dir`.
pub fn codex(config: Option<(&Path, &str)>, state_dir: &Path, home: Option<&Path>) -> Vec<Finding> {
    const CHECK: &str = "Codex";
    let Some((path, text)) = config else {
        return Vec::new();
    };
    let value: toml::Table = match text.parse() {
        Ok(value) => value,
        Err(e) => {
            return vec![Finding::new(
                Level::Unknown,
                CHECK,
                format!("{} could not be read: {e}", path.display()),
            )];
        }
    };
    let roots: Vec<&str> = value
        .get("sandbox_workspace_write")
        .and_then(|t| t.get("writable_roots"))
        .and_then(toml::Value::as_array)
        .map(|a| a.iter().filter_map(toml::Value::as_str).collect())
        .unwrap_or_default();
    // A permission profile, which Codex ignores where `sandbox_mode` is set.
    let profile = value
        .get("default_permissions")
        .and_then(toml::Value::as_str)
        .and_then(|name| value.get("permissions")?.get(name));
    let profile_writes = profile
        .and_then(|p| p.get("filesystem"))
        .and_then(toml::Value::as_table)
        .is_some_and(|fs| {
            fs.iter().any(|(entry, access)| {
                access.as_str() == Some("write") && covers(entry, state_dir, home)
            })
        });
    let mode = value.get("sandbox_mode").and_then(toml::Value::as_str);
    let dir = state_dir.display();
    let finding = match mode {
        Some("danger-full-access") => Finding::new(Level::Ok, CHECK, "its sandbox is off"),
        Some("workspace-write") if roots.iter().any(|r| covers(r, state_dir, home)) => {
            Finding::new(
                Level::Ok,
                CHECK,
                "its `writable_roots` cover the state directory",
            )
        }
        Some("workspace-write") => Finding::new(
            Level::Note,
            CHECK,
            format!(
                "`sandbox_mode = \"workspace-write\"` in {}, and no `writable_roots` covers {dir}: \
                 unless a profile or an override does, `loadmaster` fails there with exit code 125",
                path.display()
            ),
        )
        .fix(format!(
            "add to {}: [sandbox_workspace_write] writable_roots = [\"{dir}\"]",
            path.display()
        )),
        Some("read-only") => Finding::new(
            Level::Note,
            CHECK,
            format!(
                "`sandbox_mode = \"read-only\"` in {}: no command may write, so `loadmaster` \
                 fails there with exit code 125",
                path.display()
            ),
        )
        .fix(format!(
            "set sandbox_mode = \"workspace-write\" and [sandbox_workspace_write] writable_roots \
             = [\"{dir}\"] in {}",
            path.display()
        )),
        _ if profile_writes => Finding::new(
            Level::Ok,
            CHECK,
            "its permission profile lets commands write there",
        ),
        _ => Finding::new(
            Level::Unknown,
            CHECK,
            format!(
                "{} sets no sandbox that shows whether commands may write to {dir}; `loadmaster \
                 doctor` run by Codex itself tells for sure",
                path.display()
            ),
        ),
    };
    vec![finding]
}

/// Whether the repository's instruction files point agents at loadmaster.
/// `files` are the ones found (`AGENTS.md`, `CLAUDE.md`) with their text.
pub fn instructions(root: &Path, files: &[(PathBuf, String)]) -> Finding {
    const CHECK: &str = "instructions";
    const LINE: &str = "Run checks only through loadmaster: before running any, run `loadmaster \
                        guide` and follow it.";
    if let Some((path, _)) = files.iter().find(|(_, text)| text.contains("loadmaster")) {
        return Finding::new(
            Level::Ok,
            CHECK,
            format!("{} tells agents of loadmaster", path.display()),
        );
    }
    let to = match files.first() {
        Some((path, _)) => path.display().to_string(),
        None => root.join("AGENTS.md").display().to_string(),
    };
    Finding::new(
        Level::Note,
        CHECK,
        "no AGENTS.md or CLAUDE.md in the repository tells agents of loadmaster (their \
         instructions may come from elsewhere)",
    )
    .fix(format!("add this line to {to}: {LINE}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(path: &str, json: &str) -> Settings {
        Settings {
            path: PathBuf::from(path),
            value: Some(serde_json::from_str(json).unwrap()),
        }
    }

    #[test]
    fn a_path_covers_the_directories_under_it() {
        let home = Path::new("/home/u");
        let dir = Path::new("/home/u/.local/state/loadmaster");
        for entry in [
            "~/.local/state/loadmaster",
            "~/.local/state",
            "~/.local/state/**",
            "/home/u",
            "~",
        ] {
            assert!(covers(entry, dir, Some(home)), "{entry}");
        }
        for entry in ["~/.local/share", "/tmp", "~/.local/state/loadmaster-other"] {
            assert!(!covers(entry, dir, Some(home)), "{entry}");
        }
    }

    #[test]
    fn claude_codes_sandbox_is_read_as_its_layers_say() {
        let home = Path::new("/home/u");
        let dir = Path::new("/home/u/.local/state/loadmaster");
        let user = settings(
            "/home/u/.claude/settings.json",
            r#"{"sandbox": {"enabled": true}}"#,
        );
        // A wait well within the default timeout: only the sandbox counts.
        let level = |files: &[Settings]| {
            claude_code(files, dir, Some(home), Some(60))
                .iter()
                .map(|f| f.level)
                .min()
        };
        // On, and nothing lets it write: a note, with the fix.
        let found = claude_code(std::slice::from_ref(&user), dir, Some(home), Some(60));
        assert_eq!(found[0].level, Level::Note);
        assert!(
            found[0]
                .fix
                .as_deref()
                .unwrap()
                .contains("\"allowWrite\": [\"~/.local/state/loadmaster\"]"),
            "{found:?}"
        );
        // A project file's list adds to it.
        let project = settings(
            "/repo/.claude/settings.json",
            r#"{"sandbox": {"filesystem": {"allowWrite": ["~/.local/state/loadmaster"]}}}"#,
        );
        assert_eq!(level(&[user, project]), Some(Level::Ok));
        // A higher file that turns it off decides.
        let on = settings(
            "/home/u/.claude/settings.json",
            r#"{"sandbox": {"enabled": true}}"#,
        );
        let off = settings(
            "/repo/.claude/settings.local.json",
            r#"{"sandbox": {"enabled": false}}"#,
        );
        assert_eq!(level(&[on, off]), Some(Level::Ok));
    }

    #[test]
    fn a_wait_that_outlasts_claude_codes_timeout_is_noted() {
        let dir = Path::new("/s");
        let files = [settings(
            "/u.json",
            r#"{"env": {"BASH_DEFAULT_TIMEOUT_MS": "300000"}}"#,
        )];
        let level = |max_wait| claude_code(&files, dir, None, max_wait)[0].level;
        assert_eq!(level(None), Level::Note);
        assert_eq!(level(Some(300)), Level::Note);
        assert_eq!(level(Some(200)), Level::Ok);
    }

    #[test]
    fn codexs_sandbox_is_read_from_its_config() {
        let home = Path::new("/home/u");
        let dir = Path::new("/home/u/.local/state/loadmaster");
        let path = Path::new("/home/u/.codex/config.toml");
        let level = |text: &str| codex(Some((path, text)), dir, Some(home))[0].level;
        assert_eq!(level("sandbox_mode = \"workspace-write\""), Level::Note);
        assert_eq!(
            level(
                "sandbox_mode = \"workspace-write\"\n[sandbox_workspace_write]\n\
                 writable_roots = [\"/home/u/.local/state/loadmaster\"]"
            ),
            Level::Ok
        );
        assert_eq!(level("sandbox_mode = \"read-only\""), Level::Note);
        assert_eq!(
            level(
                "default_permissions = \"lm\"\n[permissions.lm]\nextends = \":workspace\"\n\
                 [permissions.lm.filesystem]\n\"~/.local/state/loadmaster\" = \"write\""
            ),
            Level::Ok
        );
        assert_eq!(level("model = \"x\""), Level::Unknown);
    }

    #[test]
    fn instruction_files_are_checked_for_loadmaster() {
        let root = Path::new("/repo");
        let agents = (
            root.join("AGENTS.md"),
            "Run tests with npm test.".to_owned(),
        );
        let found = instructions(root, std::slice::from_ref(&agents));
        assert_eq!(found.level, Level::Note);
        assert!(
            found
                .fix
                .unwrap()
                .starts_with("add this line to /repo/AGENTS.md: ")
        );
        let told = (
            root.join("CLAUDE.md"),
            "Use `loadmaster run test`.".to_owned(),
        );
        assert_eq!(instructions(root, &[agents, told]).level, Level::Ok);
    }
}
