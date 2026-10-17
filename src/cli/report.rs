//! `loadmaster report`.

use crate::*;

pub(crate) fn report(args: ReportArgs) -> Result<i32> {
    let path = config::state_dir()?.join(HISTORY_FILE);
    let (entries, unreadable) = match history::read(&path) {
        Ok(read) => read,
        Err(e) if e.kind() == io::ErrorKind::NotFound => (Vec::new(), 0),
        Err(e) => return Err(Error::io(format!("cannot read {}", path.display()), e)),
    };
    let now = history::unix_ms(SystemTime::now());
    let since = args.since.map(|time| time.at(now));
    let until = args.until.map(|time| time.at(now));
    if let (Some(since), Some(until)) = (since, until)
        && until <= since
    {
        return Err(Error::Usage("--until must be after --since".to_owned()));
    }
    let options = report::Options {
        source: &path,
        since,
        until,
        dir: args.dir.as_deref(),
        args: args.args.as_deref(),
        unreadable,
        cpus: process::online_cpus(),
    };
    if args.json {
        print(&format!("{:#}\n", report::json(&entries, &options)));
    } else {
        print(&report::render(&entries, &options));
    }
    Ok(0)
}
