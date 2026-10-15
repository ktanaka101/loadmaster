//! The admission rule (see `docs/DECISIONS.md`, "Admission"). A job decides
//! from the jobs ahead of it alone, so no job is delayed by the jobs behind
//! it; what the rule protects is the limits. A job ahead decides its own
//! start without the jobs behind it, so a job may start now only if it fits
//! now and will still fit beside every job ahead of it when that one starts:
//!
//! 1. On each dimension it uses, the running jobs ahead of it and those ahead
//!    that may start now (at what they would start with), plus it, fit
//!    within the smallest limit any of them records; on CPU also within its
//!    own limit lowered by CPU busy outside the queue.
//! 2. For every job X ahead of it that has not started, on each dimension it
//!    uses, X plus the most that other jobs ahead could use beside X, plus
//!    it, fit within the smallest limit it, X or those jobs record.
//!
//! "The jobs ahead" are the jobs ahead in line plus every running
//! measure-only job, wherever it is. The jobs are judged in one pass in line
//! order: for each job not started, what could run beside its start is built
//! up as the jobs behind it are passed, so a pass costs as many additions as
//! there are pairs of jobs.

use std::collections::BTreeMap;

use super::{
    Admission, Cause, Dim, Job, JobId, JobRef, JobState, Outside, Shortage, Wait, demands,
};

/// A dimension whose room beside a start is at most this is tracked exactly
/// when working out what could run beside it; others are bounded by the sum.
const TRACKED_ROOM: u64 = 64;
/// At most this many usage vectors are tracked for one start.
const STATES: u64 = 4096;

/// What a job may hold on each dimension: the least and the most.
type Amounts<'a> = BTreeMap<Dim<'a>, (u64, u64)>;

/// What the rule needs of a job, worked out once a pass.
struct Info<'a> {
    job: &'a Job,
    /// What it may hold: what it holds if it runs; if not, its amounts, with
    /// CPU from 1 core to its weight if it adapts to CPU outside the queue.
    amounts: Amounts<'a>,
    /// The limit it records for each dimension it uses, if any (CPU: its
    /// capacity).
    records: BTreeMap<Dim<'a>, Option<u64>>,
    /// What it holds or would start with now, and its own limit (see
    /// [`demands`]).
    now: Vec<(Dim<'a>, u64, u64)>,
}

impl<'a> Info<'a> {
    fn of(job: &'a Job, outside: Option<Outside>) -> Info<'a> {
        let base = demands(job, None);
        let amounts = base
            .iter()
            .map(|&(dim, amount, _)| {
                if dim == Dim::Cpu && job.state != JobState::Running && job.adapt_cpu {
                    (dim, (1, amount))
                } else {
                    (dim, (amount, amount))
                }
            })
            .collect();
        let records = base
            .iter()
            .map(|&(dim, _, limit)| {
                let limit = if dim == Dim::Cpu {
                    Some(u64::from(job.capacity))
                } else {
                    (limit != u64::MAX).then_some(limit)
                };
                (dim, limit)
            })
            .collect();
        Info {
            job,
            amounts,
            records,
            now: demands(job, outside),
        }
    }

    fn record(&self, dim: Dim) -> Option<u64> {
        self.records.get(&dim).copied().flatten()
    }
}

/// The rule for the job at `at` in `jobs` (in line order).
pub(super) fn decide(jobs: &[Job], at: usize, outside: Option<Outside>) -> Admission {
    pass(jobs, outside, Some(at))
        .pop()
        .map_or(Admission::Admitted, |(_, admission)| admission)
}

/// The rule for every job that has not started, in line order.
pub(super) fn explain(jobs: &[Job], outside: Option<Outside>) -> Vec<(JobId, Admission)> {
    pass(jobs, outside, None)
}

/// Judges the jobs in line order, up to `only` (whose decision is the one
/// returned, explained) or all (each explained).
fn pass(jobs: &[Job], outside: Option<Outside>, only: Option<usize>) -> Vec<(JobId, Admission)> {
    let infos: Vec<Info> = jobs.iter().map(|job| Info::of(job, outside)).collect();
    let measured: Vec<usize> = (0..jobs.len())
        .filter(|&i| jobs[i].state == JobState::Running && jobs[i].measure_only)
        .collect();
    let mut may_start = vec![false; jobs.len()];
    let mut starts: Vec<Start> = Vec::new();
    let mut out = Vec::new();
    for i in 0..only.map_or(jobs.len(), |at| at + 1) {
        let job = &jobs[i];
        if job.state == JobState::Running {
            // Measure-only jobs are beside every start already.
            if !job.measure_only {
                for start in &mut starts {
                    start.add(&infos, i);
                }
            }
            continue;
        }
        let explain = only.is_none_or(|at| at == i);
        let admission = judge(&infos, i, &may_start, &measured, &starts, outside, explain);
        if job.state == JobState::Waiting {
            may_start[i] = admission == Admission::Admitted;
        }
        if explain {
            out.push((job.id, admission));
        }
        for start in &mut starts {
            start.add(&infos, i);
        }
        starts.push(Start::new(&infos, i, &measured));
    }
    out
}

/// A job ahead that has not started, and what could run beside its start,
/// at its full CPU, of the jobs ahead of the one judged. One that adapts may
/// start with fewer cores, but only while its lowered limit is below its
/// weight, and then only with no core in use ahead of it: the jobs behind it
/// running then started fitting beside it at its full CPU, and fit beside it
/// with fewer.
struct Start<'a> {
    at: usize,
    beside: Beside<'a>,
}

impl<'a> Start<'a> {
    fn new(infos: &[Info<'a>], at: usize, measured: &[usize]) -> Start<'a> {
        let (_, hi) = infos[at].amounts.get(&Dim::Cpu).copied().unwrap_or((0, 0));
        let mut beside = Beside::new(&infos[at], at, hi);
        // The jobs ahead of it, and the measure-only ones, which are beside
        // every start.
        for i in (0..at).filter(|i| !measured.contains(i)) {
            beside.add(infos, i);
        }
        for &m in measured {
            beside.add(infos, m);
        }
        Start { at, beside }
    }

    /// Job `i`, behind this one, is now a job ahead of the ones judged next.
    fn add(&mut self, infos: &[Info<'a>], i: usize) {
        self.beside.add(infos, i);
    }
}

/// The sets of jobs that could run beside a start, built up job by job: a
/// dynamic program over the usage of the dimensions where that is bounded and
/// could bind, each state reached once (from the first job that reached it).
///
/// Everything running when X starts fits within the smallest limit any of it
/// records, X's own included, so on a dimension X records a limit for, the
/// sets are judged within that one. On another, within the largest limit the
/// jobs added so far record (unbounded once one uses it without one): a
/// relaxation of each set's own. All of it depends only on the jobs added so
/// far, which are ahead of the job judged. A dimension whose room is large,
/// or beyond the state budget, is bounded by the sum of the jobs' largest
/// amounts instead, which only makes the bound larger.
struct Beside<'a> {
    x: usize,
    x_amounts: Amounts<'a>,
    x_cpu: u64,
    /// X's own limit where it records one.
    x_limits: BTreeMap<Dim<'a>, u64>,
    /// The jobs added, in order.
    added: Vec<usize>,
    /// Those of them too big to fit beside X on their own, so far.
    unfit: Vec<usize>,
    /// The limit the sets are judged within, per dimension (`u64::MAX`:
    /// unbounded).
    within: BTreeMap<Dim<'a>, u64>,
    /// Of the jobs added that fit beside X on their own: the sum of their
    /// largest amounts, per dimension.
    sums: BTreeMap<Dim<'a>, u64>,
    /// The dimensions tracked exactly, with their room and stride.
    tracked: Vec<(Dim<'a>, u64, usize)>,
    /// For each state reached: the state and the job it was reached from.
    reached: Vec<Option<(usize, usize)>>,
    seen: Vec<usize>,
    /// Per tracked dimension, the largest value reached and its state.
    best: Vec<(u64, usize)>,
    /// The smallest limit X or a job beside it records, and whose it is.
    smallest: BTreeMap<Dim<'a>, (u64, usize)>,
    /// Whether some job beside X, or X, records a limit other than the one a
    /// dimension is judged within (a set found may not run then).
    disagree: bool,
}

impl<'a> Beside<'a> {
    fn new(x: &Info<'a>, at: usize, x_cpu: u64) -> Beside<'a> {
        let x_limits: BTreeMap<Dim, u64> = x
            .records
            .iter()
            .filter_map(|(&d, &l)| Some((d, l?)))
            .collect();
        let mut within = x_limits.clone();
        for &d in x.amounts.keys() {
            within.entry(d).or_insert(u64::MAX);
        }
        Beside {
            x: at,
            x_amounts: x.amounts.clone(),
            x_cpu,
            smallest: x_limits.iter().map(|(&d, &l)| (d, (l, at))).collect(),
            x_limits,
            added: Vec::new(),
            unfit: Vec::new(),
            within,
            sums: BTreeMap::new(),
            tracked: Vec::new(),
            reached: vec![Some((0, usize::MAX))],
            seen: vec![0],
            best: Vec::new(),
            disagree: false,
        }
    }

    fn x_amount(&self, dim: Dim) -> u64 {
        if dim == Dim::Cpu {
            self.x_cpu
        } else {
            self.x_amounts.get(&dim).map_or(0, |r| r.0)
        }
    }

    /// The room beside X on `dim`, if it is bounded.
    fn room(&self, dim: Dim) -> Option<u64> {
        let within = *self.within.get(&dim)?;
        (within != u64::MAX).then(|| within.saturating_sub(self.x_amount(dim)))
    }

    /// Whether job `i` fits beside X on its own.
    fn fits_alone(&self, infos: &[Info], i: usize) -> bool {
        infos[i]
            .amounts
            .iter()
            .all(|(&d, &(lo, _))| self.room(d).is_none_or(|room| lo <= room))
    }

    /// Job `i` may be beside X from now on.
    fn add(&mut self, infos: &[Info<'a>], i: usize) {
        let mut rebuild = false;
        for (&d, &own) in &infos[i].records {
            if self.x_limits.contains_key(&d) {
                continue;
            }
            let limit = match own {
                None => u64::MAX,
                Some(l) => self.within.get(&d).map_or(l, |&w| w.max(l)),
            };
            match self.within.insert(d, limit) {
                // A bound grew. The room of a tracked dimension changed, or a
                // job too big before may fit now: work it out again. Else
                // only a job that used it now records less than it.
                Some(before) if before != limit => {
                    rebuild |= self.tracked.iter().any(|&(t, _, _)| t == d)
                        || self.unfit.iter().any(|&u| self.fits_alone(infos, u));
                    self.disagree |= self.sums.contains_key(&d);
                }
                _ => {}
            }
        }
        self.added.push(i);
        if rebuild {
            return self.rebuild(infos);
        }
        if !self.fits_alone(infos, i) {
            self.unfit.push(i);
            return;
        }
        self.count(infos, i);
        // A dimension that now could bind and is not tracked yet: track it,
        // if the budget allows.
        let newly_binding = infos[i].amounts.keys().any(|&d| {
            !self.tracked.iter().any(|&(t, _, _)| t == d)
                && self.room(d).is_some_and(|room| {
                    room <= TRACKED_ROOM && self.sums.get(&d).is_some_and(|&sum| sum > room)
                })
        });
        if newly_binding {
            let tracked: Vec<Dim> = self.tracked.iter().map(|&(d, _, _)| d).collect();
            if self.wanted_tracked() != tracked {
                return self.rebuild(infos);
            }
        }
        self.step(infos, i);
    }

    /// Counts job `i`, which fits beside X on its own, in the sums, the
    /// smallest limits and whether the limits disagree.
    fn count(&mut self, infos: &[Info<'a>], i: usize) {
        for (&d, &(_, hi)) in &infos[i].amounts {
            *self.sums.entry(d).or_default() += hi;
        }
        for (&d, &own) in &infos[i].records {
            if let Some(l) = own {
                let entry = self.smallest.entry(d).or_insert((l, i));
                if l < entry.0 {
                    *entry = (l, i);
                }
            }
            if own.unwrap_or(u64::MAX) != self.within.get(&d).copied().unwrap_or(u64::MAX) {
                self.disagree = true;
            }
        }
    }

    /// The dimensions to track: bounded, with little room, that the jobs
    /// added could fill, smallest room first, within the state budget.
    fn wanted_tracked(&self) -> Vec<Dim<'a>> {
        let mut dims: Vec<(u64, Dim)> = self
            .sums
            .iter()
            .filter_map(|(&d, &sum)| {
                let room = self.room(d)?;
                (room <= TRACKED_ROOM && sum > room).then_some((room, d))
            })
            .collect();
        dims.sort();
        let mut size: u64 = 1;
        let mut wanted = Vec::new();
        for (room, d) in dims {
            let next = size.saturating_mul(room + 1);
            if next <= STATES {
                wanted.push(d);
                size = next;
            }
        }
        wanted.sort();
        wanted
    }

    /// Works everything out again from the jobs added.
    fn rebuild(&mut self, infos: &[Info<'a>]) {
        let added = std::mem::take(&mut self.added);
        self.sums.clear();
        self.smallest = self
            .x_limits
            .iter()
            .map(|(&d, &l)| (d, (l, self.x)))
            .collect();
        self.disagree = infos[self.x].records.iter().any(|(&d, &own)| {
            own.unwrap_or(u64::MAX) != self.within.get(&d).copied().unwrap_or(u64::MAX)
        });
        let (fitting, unfit): (Vec<usize>, Vec<usize>) =
            added.iter().partition(|&&i| self.fits_alone(infos, i));
        self.unfit = unfit;
        for &i in &fitting {
            self.count(infos, i);
        }
        let mut size = 1usize;
        self.tracked = self
            .wanted_tracked()
            .into_iter()
            .map(|d| {
                let room = self.room(d).expect("tracked dimensions are bounded");
                let stride = size;
                size *= room as usize + 1;
                (d, room, stride)
            })
            .collect();
        self.reached = vec![None; size];
        self.reached[0] = Some((0, usize::MAX));
        self.seen = vec![0];
        self.best = vec![(0, 0); self.tracked.len()];
        for &i in &fitting {
            self.step(infos, i);
        }
        self.added = added;
    }

    /// Adds job `i` to the sets reached so far. Only CPU can be a range.
    fn step(&mut self, infos: &[Info], i: usize) {
        if self.tracked.is_empty() {
            return;
        }
        let amounts = &infos[i].amounts;
        let mut fixed = Vec::with_capacity(self.tracked.len());
        let mut range = None;
        for &(d, room, stride) in &self.tracked {
            let (lo, hi) = amounts.get(&d).copied().unwrap_or((0, 0));
            let room = room as usize;
            if lo == hi {
                fixed.push((stride, room, lo as usize));
            } else {
                range = Some((stride, room, lo as usize, hi as usize));
            }
        }
        let before = self.seen.len();
        for s in 0..before {
            let state = self.seen[s];
            let mut base = state;
            let fits = fixed.iter().all(|&(stride, room, amount)| {
                base += amount * stride;
                state / stride % (room + 1) + amount <= room
            });
            if !fits {
                continue;
            }
            let (stride, room, lo, hi) = range.unwrap_or((1, 0, 0, 0));
            let at = if range.is_some() {
                state / stride % (room + 1)
            } else {
                0
            };
            for v in lo..=hi {
                if range.is_some() && at + v > room {
                    break;
                }
                let next = base + v * stride * usize::from(range.is_some());
                if self.reached[next].is_none() {
                    self.reached[next] = Some((state, i));
                    self.seen.push(next);
                    for (t, &(_, room, stride)) in self.tracked.iter().enumerate() {
                        let value = (next / stride % (room as usize + 1)) as u64;
                        if value > self.best[t].0 {
                            self.best[t] = (value, next);
                        }
                    }
                }
            }
        }
    }

    /// The dimensions on which the jobs beside X could be too many for the
    /// room, whether tracked or not.
    fn binding(&self) -> impl Iterator<Item = Dim<'a>> + '_ {
        self.sums
            .iter()
            .filter(|&(&d, &sum)| self.room(d).is_some_and(|room| sum > room))
            .map(|(&d, _)| d)
    }

    /// The most the jobs added so far could use of `dim` beside X.
    fn heaviest(&self, dim: Dim) -> u64 {
        if let Some(t) = self.tracked.iter().position(|&(d, _, _)| d == dim) {
            return self.best[t].0;
        }
        let sum = self.sums.get(&dim).copied().unwrap_or(0);
        self.room(dim).map_or(sum, |room| sum.min(room))
    }

    /// A set of the jobs added that uses `heaviest(dim)` of `dim` beside X
    /// and can run, if one was found.
    fn witness(&self, infos: &[Info], dim: Dim) -> Option<Vec<usize>> {
        let tracked = |d: &Dim| self.tracked.iter().any(|(t, _, _)| t == d);
        let exact = !self.disagree && self.binding().all(|d| tracked(&d));
        if let Some(t) = self.tracked.iter().position(|&(d, _, _)| d == dim) {
            let (value, mut state) = self.best[t];
            if value != 0 && !exact {
                return None;
            }
            let mut set = Vec::new();
            while state != 0 {
                let (prev, job) = self.reached[state].expect("a state reached");
                set.push(job);
                state = prev;
            }
            set.reverse();
            return Some(set);
        }
        // With nothing binding, every job that fits on its own fits beside X
        // with all the others: that is the set, and the sum is exact.
        if self.heaviest(dim) == 0 {
            Some(Vec::new())
        } else if exact && self.binding().next().is_none() {
            Some(
                self.added
                    .iter()
                    .copied()
                    .filter(|&i| self.fits_alone(infos, i))
                    .collect(),
            )
        } else {
            None
        }
    }
}

/// The rule for the job at `at`, given which jobs before it may start now
/// and what could run beside each start ahead of it. With `explain`, every
/// shortage is worked out; without, the first ends it.
fn judge(
    infos: &[Info],
    at: usize,
    may_start: &[bool],
    measured: &[usize],
    starts: &[Start],
    outside: Option<Outside>,
    explain: bool,
) -> Admission {
    let k = &infos[at];
    let mut causes: BTreeMap<Dim, Cause> = BTreeMap::new();

    // 1. Now: the running jobs ahead, those that may start now, and the
    // measure-only ones.
    let now: Vec<usize> = (0..at)
        .filter(|&i| {
            let job = infos[i].job;
            (job.state == JobState::Running && !job.measure_only) || may_start[i]
        })
        .chain(measured.iter().copied().filter(|&i| i != at))
        .collect();
    for &(dim, amount, own) in &k.now {
        let mut used = 0;
        let mut by = Vec::new();
        let mut limit = k.record(dim).map_or((u64::MAX, None), |l| (l, None));
        for &i in &now {
            let info = &infos[i];
            if let Some(&(_, a, _)) = info.now.iter().find(|(d, _, _)| *d == dim) {
                used += a;
                by.push(JobRef::of(info.job));
            }
            if let Some(l) = info.record(dim)
                && l < limit.0
            {
                limit = (l, Some(JobRef::of(info.job)));
            }
        }
        let (mut limit, mut limit_by) = limit;
        let mut lowered = None;
        if dim == Dim::Cpu && own < limit {
            (limit, limit_by) = (own, None);
            lowered = outside.map(|o| o.lowered());
        }
        if used + amount > limit {
            if !explain {
                return waiting(infos, at, Vec::new());
            }
            causes.insert(
                dim,
                Cause::InUse {
                    used,
                    limit,
                    by,
                    limit_by,
                    outside: lowered,
                },
            );
        }
    }

    // 2. Beside every start ahead.
    for start in starts {
        let x = &infos[start.at];
        let beside = &start.beside;
        for &(dim, amount, _) in &k.now {
            if causes.contains_key(&dim) {
                continue;
            }
            let (mut limit, mut limit_job) = beside
                .smallest
                .get(&dim)
                .map_or((u64::MAX, None), |&(l, i)| (l, Some(i)));
            if let Some(l) = k.record(dim)
                && l <= limit
            {
                (limit, limit_job) = (l, None);
            }
            if limit == u64::MAX {
                continue;
            }
            let used = beside.heaviest(dim) + beside.x_amount(dim);
            if used + amount > limit {
                if !explain {
                    return waiting(infos, at, Vec::new());
                }
                causes.insert(
                    dim,
                    Cause::BesideStart {
                        start: JobRef::of(x.job),
                        used,
                        limit,
                        beside: beside
                            .witness(infos, dim)
                            .map(|set| set.iter().map(|&i| JobRef::of(infos[i].job)).collect()),
                        limit_by: limit_job
                            .filter(|&i| i != at)
                            .map(|i| JobRef::of(infos[i].job)),
                    },
                );
            }
        }
    }

    if causes.is_empty() {
        return Admission::Admitted;
    }
    let short = k
        .now
        .iter()
        .filter_map(|&(dim, amount, _)| {
            Some(Shortage {
                need: dim.need(),
                amount,
                cause: causes.remove(&dim)?,
            })
        })
        .collect();
    waiting(infos, at, short)
}

fn waiting(infos: &[Info], at: usize, short: Vec<Shortage>) -> Admission {
    let ahead = infos[..at]
        .iter()
        .filter(|info| info.job.state != JobState::Running)
        .count();
    Admission::Waiting(Wait {
        ahead: u32::try_from(ahead).unwrap_or(u32::MAX),
        short,
    })
}
