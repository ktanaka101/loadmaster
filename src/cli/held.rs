//! `loadmaster held`, and the check of a holder that nested runs share.

use crate::*;

/// Whether this process runs inside a job started from this checkout: the
/// check made for nested runs, plus the checkout. Silent when it does, so a
/// script can use it as a guard.
pub(crate) fn held(args: HeldArgs) -> Result<i32> {
    let cwd = env::current_dir().map_err(|e| Error::io("cannot read the current directory", e))?;
    let checkout = RepoConfig::locate(&cwd)?;
    let not_held = |reason: String| -> Result<i32> {
        say!(
            "loadmaster: not running through loadmaster: {reason}\n\
             loadmaster: start it with `loadmaster run <task>`; the tasks are in {}",
            checkout.display()
        );
        Ok(EXIT_NOT_HELD)
    };
    let Some(value) = env::var_os(HELD_ENV).filter(|v| !v.is_empty()) else {
        return not_held(format!("{HELD_ENV} is not set"));
    };
    let job = match holder_here(&value.to_string_lossy(), &checkout)? {
        Ok(job) => job,
        Err(reason) => return not_held(reason),
    };
    if let Some(missing) = args
        .uses
        .iter()
        .find(|name| !job.uses.iter().any(|u| &u.name == *name))
    {
        return not_held(format!(
            "job #{} (`{}`) does not use `{missing}`",
            job.id, job.task
        ));
    }
    Ok(0)
}

/// The job that `value` (a LOADMASTER_HELD) names, if it is running and was
/// started from the checkout of `config`, this checkout's .loadmaster.toml;
/// otherwise why not. Both `held` and nested runs check this.
pub(crate) fn holder_here(value: &str, config: &Path) -> Result<std::result::Result<Job, String>> {
    let (_, queue) = open_queue()?;
    let job = match value.trim().parse::<JobId>() {
        Ok(id) => queue.holder(id)?,
        Err(_) => None,
    };
    let Some(job) = job else {
        return Ok(Err(format!(
            "{HELD_ENV}={value} does not refer to a running job"
        )));
    };
    // A checkout is the resolved directory of its .loadmaster.toml, as for
    // per-checkout limits: a symlink to the same checkout matches, a worktree
    // whose config is a symlink to another's file does not.
    let here = config.parent().and_then(|dir| fs::canonicalize(dir).ok());
    if here.as_ref() != Some(&job.checkout) {
        return Ok(Err(format!(
            "job #{} (`{}`) was started from another checkout, in {}",
            job.id,
            job.task,
            job.dir.display()
        )));
    }
    Ok(Ok(job))
}
