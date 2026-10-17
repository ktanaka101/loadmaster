use std::io;
use std::path::{Path, PathBuf};

use crate::config::REPO_CONFIG_FILE;
use crate::queue::JobId;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}", not_found(start, repo_root.as_deref(), beyond.as_deref()))]
    RepoConfigNotFound {
        start: PathBuf,
        repo_root: Option<PathBuf>,
        beyond: Option<PathBuf>,
    },

    #[error("cannot read {}: {source}", path.display())]
    ReadConfig { path: PathBuf, source: io::Error },

    #[error("LOADMASTER_CONFIG names {}, which does not exist", path.display())]
    MissingMachineConfig { path: PathBuf },

    #[error("invalid config {}: {message}", path.display())]
    InvalidConfig { path: PathBuf, message: String },

    #[error(
        "{} needs loadmaster {required} or newer (its `min_version`), but this is {}: \
         upgrade loadmaster",
        path.display(),
        crate::config::VERSION
    )]
    VersionTooOld { path: PathBuf, required: String },

    #[error(
        "unknown task `{name}` in {}{}\ntasks: {}",
        path.display(),
        closest(name, available).map_or(String::new(), |t| format!("; did you mean `{t}`?")),
        format_tasks(available)
    )]
    UnknownTask {
        name: String,
        path: PathBuf,
        /// Each task's name, weight and description.
        available: Vec<(String, u32, Option<String>)>,
    },

    #[error(
        "{reason}\n\
         `loadmaster run` was started with LOADMASTER_HELD set, so it would run in that job's slot; \
         unset LOADMASTER_HELD if it leaked into this environment"
    )]
    InvalidHeld { reason: String },

    #[error(
        "{reason}\n\
         a nested `loadmaster run` uses the slot of the job that holds it, and waiting for more \
         from inside that slot could deadlock"
    )]
    NestedDoesNotFit { reason: String },

    #[error(
        "{reason}\n\
         a nested `loadmaster run` runs in the process group of the job that holds it, and is \
         stopped as that job is"
    )]
    NestedStopSignal { reason: String },

    #[error("{var}={value} is invalid: expected {expected}")]
    InvalidEnv {
        var: &'static str,
        value: String,
        expected: &'static str,
    },

    #[error("{0}")]
    Usage(String),

    #[error("cannot determine the home directory: HOME is not set")]
    NoHome,

    #[error(
        "cannot write to the state directory {} ({detail})\n\
         loadmaster keeps the queue that every agent on this machine shares there; in an \
         agent's sandbox, it must be allowed to write to it. LOADMASTER_STATE_DIR moves it, \
         but every agent must then use the same one",
        dir.display()
    )]
    StateDirNotWritable { dir: PathBuf, detail: String },

    #[error("cannot write to the queue database {}: check its permissions", path.display())]
    QueueReadOnly { path: PathBuf },

    #[error(
        "the queue {} has layout {layout} and admission protocol {}, which loadmaster {} does \
         not know: a later loadmaster changed it; give every agent on this machine that version",
        path.display(),
        protocol.map_or("none".to_owned(), |p| p.to_string()),
        env!("CARGO_PKG_VERSION")
    )]
    QueueTooNew {
        path: PathBuf,
        layout: i64,
        protocol: Option<i64>,
    },
    #[error(
        "the queue is moving to the admission protocol of loadmaster 0.15 and later: {left} \
         job(s) of loadmaster 0.12 to 0.14 have to finish first"
    )]
    QueueMoving { left: usize },
    #[error("job #{0} disappeared from the queue")]
    JobVanished(JobId),

    #[error("cannot stop job #{id}: {why}: {source}{}", sandbox_hint(source))]
    CannotSignal {
        id: JobId,
        why: String,
        source: io::Error,
    },

    #[error("state database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("{context}: {source}")]
    Io { context: String, source: io::Error },
}

impl Error {
    pub fn io(context: impl Into<String>, source: io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }

    /// What to do about it, where there is one thing to do: what an agent
    /// is told on a `loadmaster: next:` line.
    pub fn next(&self) -> Option<String> {
        Some(match self {
            Error::RepoConfigNotFound { .. } => format!(
                "run checks from a checkout that has {REPO_CONFIG_FILE}; `loadmaster guide` \
                 says how loadmaster is used and set up"
            ),
            Error::UnknownTask {
                name, available, ..
            } => match closest(name, available) {
                Some(task) => format!("run `loadmaster run {task}`"),
                None => "run one of the tasks above with `loadmaster run <task>`".to_owned(),
            },
            Error::VersionTooOld { required, .. } => {
                format!("install loadmaster {required} or newer, then run the same command again")
            }
            Error::InvalidConfig { path, .. } => format!(
                "fix {}; `loadmaster check` says whether it is valid",
                path.display()
            ),
            Error::MissingMachineConfig { path } => format!(
                "create {} or unset LOADMASTER_CONFIG, then run the same command again",
                path.display()
            ),
            Error::StateDirNotWritable { dir, .. } => format!(
                "allow writing to {} in the settings of the tool that runs this command \
                 (`loadmaster doctor` checks them), then run the same command again",
                dir.display()
            ),
            Error::QueueTooNew { .. } => "upgrade loadmaster to the version that the other \
                 agents on this machine use, then run the same command again"
                .to_owned(),
            Error::InvalidEnv { var, .. } => {
                format!("fix or unset {var}, then run the same command again")
            }
            _ => return None,
        })
    }
}

/// The task meant by a mistyped `name`, if one is close: within an edit or
/// two (a third of its length, at least one), or the start of one.
fn closest<'a>(name: &str, tasks: &'a [(String, u32, Option<String>)]) -> Option<&'a str> {
    let allowed = (name.chars().count() / 3).max(1);
    tasks
        .iter()
        .map(|(task, _, _)| (distance(name, task), task.as_str()))
        .filter(|&(d, task)| d <= allowed || (name.len() >= 2 && task.starts_with(name)))
        .min_by_key(|&(d, _)| d)
        .map(|(_, task)| task)
}

/// The edit distance between `a` and `b`, in characters, where swapping two
/// neighbours (`lnit` for `lint`) is one edit, as typing slips often are.
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

fn not_found(start: &Path, repo_root: Option<&Path>, beyond: Option<&Path>) -> String {
    let mut message = match repo_root {
        Some(root) if root == start => format!(
            "no {REPO_CONFIG_FILE} found in {}, the root of its repository",
            start.display()
        ),
        Some(root) => format!(
            "no {REPO_CONFIG_FILE} found in {} or its parents up to the root of its repository, {}",
            start.display(),
            root.display()
        ),
        None => format!(
            "no {REPO_CONFIG_FILE} found in {} or any parent directory",
            start.display()
        ),
    };
    if let Some(beyond) = beyond {
        message += &format!(
            "\n{} is outside this repository (in the main checkout a worktree lives in, \
             for example) and is not used: commit {REPO_CONFIG_FILE} on this branch",
            beyond.display()
        );
    }
    message
}

fn sandbox_hint(source: &io::Error) -> &'static str {
    if source.kind() == io::ErrorKind::PermissionDenied {
        "\nin an agent's sandbox, a command may not signal processes that another command \
         started: run `loadmaster cancel` outside the sandbox, or from where the job was started"
    } else {
        ""
    }
}

fn format_tasks(tasks: &[(String, u32, Option<String>)]) -> String {
    if tasks.is_empty() {
        return "(none defined)".to_owned();
    }
    tasks
        .iter()
        .map(|(name, weight, description)| match description {
            Some(text) => format!("{name} (weight {weight}: {text})"),
            None => format!("{name} (weight {weight})"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mistyped_task_is_matched_to_the_closest() {
        let tasks: Vec<(String, u32, Option<String>)> = ["e2e", "lint", "test", "typecheck"]
            .iter()
            .map(|t| ((*t).to_owned(), 1, None))
            .collect();
        for (typed, meant) in [
            ("lnit", Some("lint")),
            ("tset", Some("test")),
            ("tests", Some("test")),
            ("type", Some("typecheck")),
            ("e2", Some("e2e")),
            ("build", None),
        ] {
            assert_eq!(closest(typed, &tasks), meant, "{typed}");
        }
    }
}
