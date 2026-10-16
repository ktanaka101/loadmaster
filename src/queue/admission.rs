//! The admission rule (see `docs/DECISIONS.md`, "Admission"). A job decides
//! from the jobs ahead of it alone, so no job is delayed by the jobs behind
//! it; what the rule protects is the limits. A job ahead decides its own
//! start without the jobs behind it, so a job may start now, with some
//! number of cores from the least it may start with to its weight, only if
//! it fits now and will still fit beside every job ahead of it when that one
//! starts:
//!
//! 1. On each dimension it uses, the running jobs ahead of it, plus it, fit
//!    within the smallest limit any of them records; on CPU also within its
//!    own limit lowered by CPU busy outside the queue (never below the least
//!    it may start with).
//! 2. For every job X ahead of it that has not started, on each dimension it
//!    uses, X plus the most that other jobs ahead could use beside X, plus
//!    it, fit within the smallest limit it, X or those jobs record. X may
//!    start with the least cores it may: the room beside it is worked out at
//!    that, and X is counted at its weight.
//!
//! It starts with the most cores both allow. The jobs are judged in one pass
//! in line order, and a waiting job that may start counts as running, with
//! what it starts with, for the jobs behind it: the process that admits jobs
//! admits all of these at once (see [`plan`]).
//!
//! "The jobs ahead" are the jobs ahead in line plus every running
//! measure-only job, wherever it is. For each kind of job not started (its
//! amounts and limits), what could run beside its start is built up as the
//! jobs behind it are passed, so a pass costs as many additions as there are
//! jobs times kinds.

use std::cell::RefCell;
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
    /// CPU from the least it may start with to its weight.
    amounts: Amounts<'a>,
    /// The limit it records for each dimension it uses, if any (CPU: its
    /// capacity).
    records: BTreeMap<Dim<'a>, Option<u64>>,
    /// What it holds if it runs, and on CPU its own limit, lowered by CPU
    /// busy outside the queue if it adapts to it (see [`super::cpu_demand`]).
    now: Vec<(Dim<'a>, u64, u64)>,
}

impl<'a> Info<'a> {
    fn of(job: &'a Job, outside: Option<Outside>) -> Info<'a> {
        Info::with(job, outside, None)
    }

    /// A job that a pass lets start with `cores`: it runs with them from
    /// then on, for the jobs behind it.
    fn started(job: &'a Job, cores: u64) -> Info<'a> {
        Info::with(job, None, Some(cores))
    }

    fn with(job: &'a Job, outside: Option<Outside>, started: Option<u64>) -> Info<'a> {
        let base = demands(job, None);
        let least = u64::from(job.least());
        let amounts = base
            .iter()
            .map(|&(dim, amount, _)| match (dim, started) {
                (Dim::Cpu, Some(cores)) => (dim, (cores, cores)),
                (Dim::Cpu, None) if job.state != JobState::Running => {
                    (dim, (least.min(amount), amount))
                }
                _ => (dim, (amount, amount)),
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
        let mut now = demands(job, outside);
        if let Some(cores) = started {
            for entry in now.iter_mut().filter(|(d, _, _)| *d == Dim::Cpu) {
                *entry = (Dim::Cpu, cores, u64::from(job.capacity));
            }
        }
        Info {
            job,
            amounts,
            records,
            now,
        }
    }

    fn record(&self, dim: Dim) -> Option<u64> {
        self.records.get(&dim).copied().flatten()
    }

    /// The least and the most cores it may hold.
    fn cpu(&self) -> (u64, u64) {
        self.amounts.get(&Dim::Cpu).copied().unwrap_or((0, 0))
    }

    /// Whether the two are jobs of a kind, as far as the rule goes: the same
    /// amounts and limits.
    fn like(&self, other: &Info) -> bool {
        self.amounts == other.amounts && self.records == other.records
    }
}

/// The rule's decision for one job not running, and if it may start, the
/// cores it starts with.
#[derive(Debug, PartialEq)]
pub(super) struct Decision {
    pub id: JobId,
    pub admission: Admission,
    pub cores: Option<u32>,
}

/// The rule for the job at `at` in `jobs` (in line order).
pub(super) fn decide(jobs: &[Job], at: usize, outside: Option<Outside>) -> Decision {
    pass(jobs, outside, Some(at), true)
        .pop()
        .unwrap_or_else(|| Decision {
            id: jobs[at].id,
            admission: Admission::Admitted,
            cores: None,
        })
}

/// The rule for every job that has not started, in line order.
pub(super) fn explain(jobs: &[Job], outside: Option<Outside>) -> Vec<Decision> {
    pass(jobs, outside, None, true)
}

/// The waiting jobs to start now, with the cores each starts with, in line
/// order: each counts as running for the jobs behind it, so they are to start
/// together, in one transaction. Measure-only jobs start by themselves.
pub(super) fn plan(jobs: &[Job], outside: Option<Outside>) -> Vec<(JobId, u32)> {
    explain(jobs, outside)
        .into_iter()
        .filter_map(|d| {
            let job = jobs.iter().find(|job| job.id == d.id)?;
            (job.state == JobState::Waiting && !job.measure_only).then_some((d.id, d.cores?))
        })
        .collect()
}

/// [`explain`], working out the start of every job not started, as if no two
/// were of a kind: what the rule must agree with.
#[cfg(test)]
pub(super) fn explain_every_start(jobs: &[Job], outside: Option<Outside>) -> Vec<Decision> {
    pass(jobs, outside, None, false)
}

/// Judges the jobs in line order, up to `only` (whose decision is the one
/// returned, explained) or all (each explained). A waiting job that may start
/// counts as running, with the cores it starts with, for the jobs behind it.
/// With `by_kind`, one start stands for every later one of its kind (see
/// [`Info::like`]): the jobs beside the later one are those beside the first,
/// with the two swapped, so its condition is the same.
fn pass(
    jobs: &[Job],
    outside: Option<Outside>,
    only: Option<usize>,
    by_kind: bool,
) -> Vec<Decision> {
    let mut infos: Vec<Info> = jobs.iter().map(|job| Info::of(job, outside)).collect();
    let measured: Vec<usize> = (0..jobs.len())
        .filter(|&i| jobs[i].state == JobState::Running && jobs[i].measure_only)
        .collect();
    let mut running: Vec<bool> = jobs
        .iter()
        .map(|job| job.state == JobState::Running && !job.measure_only)
        .collect();
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
        let (admission, cores) = judge(&infos, i, &running, &measured, &starts, outside, explain);
        let cores = (admission == Admission::Admitted).then_some(cores);
        if explain {
            out.push(Decision {
                id: job.id,
                admission,
                cores: cores.map(|c| u32::try_from(c).unwrap_or(u32::MAX)),
            });
        }
        // It starts with these, together with the others that may.
        let starts_now = job.state == JobState::Waiting && !job.measure_only;
        if let Some(cores) = cores.filter(|_| starts_now) {
            infos[i] = Info::started(job, cores);
            running[i] = true;
        }
        for start in &mut starts {
            start.add(&infos, i);
        }
        let started = running[i];
        if !started && !(by_kind && starts.iter().any(|s| infos[s.at].like(&infos[i]))) {
            starts.push(Start::new(&infos, i, &measured));
        }
    }
    out
}

/// A job ahead that has not started, and what could run beside its start, of
/// the jobs ahead of the one judged: the room beside it is worked out at the
/// least cores it may start with, and it counts at its weight.
struct Start<'a> {
    at: usize,
    beside: Beside<'a>,
}

impl<'a> Start<'a> {
    fn new(infos: &[Info<'a>], at: usize, measured: &[usize]) -> Start<'a> {
        let mut beside = Beside::new(&infos[at], at);
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
/// relaxation of each set's own. A job that does not fit beside X within the
/// limits either of them records never runs with it, and is left out. All of
/// it depends only on the jobs added so far, which are ahead of the job
/// judged.
///
/// The states are the usage of the dimensions that could bind (the jobs could
/// use more than the room) and have little room, within a budget. Any other
/// dimension is bounded by the sum of the jobs' largest amounts and the room,
/// which only makes the bound larger; where that is not enough, [`most`]
/// works out the most over the sets that fit on the tracked dimensions.
///
/// [`most`]: Beside::most
struct Beside<'a> {
    x: usize,
    x_amounts: Amounts<'a>,
    /// The least cores X may start with, which the room beside it is worked
    /// out at, and the most, which it counts at.
    x_least: u64,
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
    /// How many times the states were worked out again, and what
    /// [`Beside::most`] found since.
    generation: u64,
    most: RefCell<BTreeMap<Dim<'a>, Most>>,
}

/// The most of a dimension not tracked that the sets reaching each state
/// could use, through the first `through` jobs added, in `generation` of the
/// states.
struct Most {
    generation: u64,
    through: usize,
    values: Vec<Option<u64>>,
    seen: Vec<usize>,
    best: u64,
}

impl<'a> Beside<'a> {
    fn new(x: &Info<'a>, at: usize) -> Beside<'a> {
        let (x_least, x_cpu) = x.cpu();
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
            x_least,
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
            generation: 0,
            most: RefCell::new(BTreeMap::new()),
        }
    }

    /// What X is counted at on `dim`: its weight on CPU.
    fn x_amount(&self, dim: Dim) -> u64 {
        if dim == Dim::Cpu {
            self.x_cpu
        } else {
            self.x_amounts.get(&dim).map_or(0, |r| r.0)
        }
    }

    /// The least X may hold on `dim`, which what could run beside it is
    /// worked out at: whatever runs when X starts fits beside that.
    fn x_least(&self, dim: Dim) -> u64 {
        if dim == Dim::Cpu {
            self.x_least
        } else {
            self.x_amount(dim)
        }
    }

    /// The room beside X on `dim`, if it is bounded.
    fn room(&self, dim: Dim) -> Option<u64> {
        let within = *self.within.get(&dim)?;
        (within != u64::MAX).then(|| within.saturating_sub(self.x_least(dim)))
    }

    /// Whether job `i` fits beside X on its own, within the limits both
    /// record.
    fn fits_alone(&self, infos: &[Info], i: usize) -> bool {
        infos[i].amounts.iter().all(|(&d, &(lo, _))| {
            self.room(d).is_none_or(|room| lo <= room)
                && infos[i]
                    .record(d)
                    .is_none_or(|own| lo + self.x_least(d) <= own)
        })
    }

    /// Job `i` may be beside X from now on.
    fn add(&mut self, infos: &[Info<'a>], i: usize) {
        let mut rebuild = false;
        let mut grew = false;
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
                    let tracked = self.tracked.iter().any(|&(t, _, _)| t == d);
                    rebuild |= tracked || self.unfit.iter().any(|&u| self.fits_alone(infos, u));
                    self.disagree |= self.sums.contains_key(&d);
                    // One that could bind, left out for the budget, may fit in
                    // it now.
                    grew |= !tracked
                        && self.room(d).is_some_and(|room| {
                            room <= TRACKED_ROOM && self.sums.get(&d).is_some_and(|&sum| sum > room)
                        });
                }
                _ => {}
            }
        }
        self.added.push(i);
        if rebuild {
            return self.rebuild(infos);
        }
        let fits = self.fits_alone(infos, i);
        if fits {
            self.count(infos, i);
        } else {
            self.unfit.push(i);
        }
        // The dimensions to track are worked out from the jobs added alone,
        // whatever their order: they may change when a room grew, or when a
        // dimension not tracked now could bind.
        let changed = grew
            || fits
                && infos[i].amounts.keys().any(|&d| {
                    !self.tracked.iter().any(|&(t, _, _)| t == d)
                        && self.room(d).is_some_and(|room| {
                            room <= TRACKED_ROOM && self.sums.get(&d).is_some_and(|&sum| sum > room)
                        })
                });
        if changed {
            let tracked: Vec<Dim> = self.tracked.iter().map(|&(d, _, _)| d).collect();
            if self.wanted_tracked() != tracked {
                return self.rebuild(infos);
            }
        }
        if fits {
            self.step(infos, i);
        }
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
        self.generation += 1;
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

    /// How a job with `amounts` moves a state, on the tracked dimensions: by
    /// its fixed amounts, as (stride, room, amount), and by any amount within
    /// its range on the one it has one on (only CPU can be a range).
    fn moves(&self, amounts: &Amounts) -> Moves {
        let mut moves = Moves {
            fixed: Vec::with_capacity(self.tracked.len()),
            range: None,
        };
        for &(d, room, stride) in &self.tracked {
            let (lo, hi) = amounts.get(&d).copied().unwrap_or((0, 0));
            let room = room as usize;
            if lo == hi {
                moves.fixed.push((stride, room, lo as usize));
            } else {
                moves.range = Some((stride, room, lo as usize, hi as usize));
            }
        }
        moves
    }

    /// Adds job `i` to the sets reached so far.
    fn step(&mut self, infos: &[Info], i: usize) {
        if self.tracked.is_empty() {
            return;
        }
        let moves = self.moves(&infos[i].amounts);
        let mut next = Vec::new();
        for s in 0..self.seen.len() {
            let state = self.seen[s];
            moves.from(state, &mut next);
            for &n in &next {
                if self.reached[n].is_none() {
                    self.reached[n] = Some((state, i));
                    self.seen.push(n);
                    for (t, &(_, room, stride)) in self.tracked.iter().enumerate() {
                        let value = (n / stride % (room as usize + 1)) as u64;
                        if value > self.best[t].0 {
                            self.best[t] = (value, n);
                        }
                    }
                }
            }
        }
    }

    /// The most of `dim`, which is not tracked, that the jobs added could
    /// use in a set that fits on the tracked dimensions, worked out from them
    /// alone: tighter than its sum where they could not all run together.
    fn most(&self, infos: &[Info], dim: Dim<'a>) -> u64 {
        let mut cache = self.most.borrow_mut();
        let most = cache.entry(dim).or_insert_with(|| Most {
            generation: u64::MAX,
            through: 0,
            values: Vec::new(),
            seen: Vec::new(),
            best: 0,
        });
        // Since the states were worked out again, start over; else go on
        // from the jobs added since the last time.
        if most.generation != self.generation {
            *most = Most {
                generation: self.generation,
                through: 0,
                values: vec![None; self.reached.len()],
                seen: vec![0],
                best: 0,
            };
            most.values[0] = Some(0);
        }
        let mut next = Vec::new();
        let mut reach: Vec<(usize, u64)> = Vec::new();
        for &i in &self.added[most.through..] {
            // One too big on its own now would start the states over when it
            // fit, as the bound it is too big for can only grow.
            if !self.fits_alone(infos, i) {
                continue;
            }
            let moves = self.moves(&infos[i].amounts);
            let amount = infos[i].amounts.get(&dim).map_or(0, |&(_, hi)| hi);
            // From the sets before this job only: it is added to each once.
            reach.clear();
            for &state in &most.seen {
                let value = most.values[state].expect("a state reached") + amount;
                moves.from(state, &mut next);
                reach.extend(next.iter().map(|&n| (n, value)));
            }
            for &(n, value) in &reach {
                match most.values[n] {
                    None => {
                        most.values[n] = Some(value);
                        most.seen.push(n);
                    }
                    Some(kept) if kept < value => most.values[n] = Some(value),
                    Some(_) => {}
                }
                most.best = most.best.max(value);
            }
        }
        most.through = self.added.len();
        most.best
    }

    /// The dimensions on which the jobs beside X could be too many for the
    /// room, whether tracked or not.
    fn binding(&self) -> impl Iterator<Item = Dim<'a>> + '_ {
        self.sums
            .iter()
            .filter(|&(&d, &sum)| self.room(d).is_some_and(|room| sum > room))
            .map(|(&d, _)| d)
    }

    /// Whether `dim` is bounded by its sum alone while others are tracked,
    /// so that [`Beside::most`] may find less.
    fn untracked(&self, dim: Dim) -> bool {
        !self.tracked.is_empty() && !self.tracked.iter().any(|&(d, _, _)| d == dim)
    }

    /// The most that could be in use of `dim`, not tracked, when X starts:
    /// X's amount and [`Beside::most`], within the room.
    fn used(&self, infos: &[Info], dim: Dim<'a>) -> u64 {
        let most = self.most(infos, dim);
        self.room(dim).map_or(most, |room| most.min(room)) + self.x_amount(dim)
    }

    /// The most the jobs added so far could use of `dim` beside X, bounded
    /// by its sum alone where it is not tracked.
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

/// How a job moves the states of a [`Beside`] (see [`Beside::moves`]).
struct Moves {
    fixed: Vec<(usize, usize, usize)>,
    range: Option<(usize, usize, usize, usize)>,
}

impl Moves {
    /// The states it reaches from `state`, into `out`: none if it does not
    /// fit there.
    fn from(&self, state: usize, out: &mut Vec<usize>) {
        out.clear();
        let mut base = state;
        let fits = self.fixed.iter().all(|&(stride, room, amount)| {
            base += amount * stride;
            state / stride % (room + 1) + amount <= room
        });
        if !fits {
            return;
        }
        match self.range {
            None => out.push(base),
            Some((stride, room, lo, hi)) => {
                let at = state / stride % (room + 1);
                out.extend(
                    (lo..=hi)
                        .take_while(|v| at + v <= room)
                        .map(|v| base + v * stride),
                );
            }
        }
    }
}

/// The rule for the job at `at`, given which jobs before it may start now
/// and what could run beside each start ahead of it. With `explain`, every
/// shortage is worked out; without, the first ends it.
fn judge<'a>(
    infos: &[Info<'a>],
    at: usize,
    running: &[bool],
    measured: &[usize],
    starts: &[Start<'a>],
    outside: Option<Outside>,
    explain: bool,
) -> (Admission, u64) {
    let k = &infos[at];
    let (least, most) = k.cpu();
    // What it needs on each dimension to start: on CPU the least it may
    // start with. The room left on CPU, which it starts with up to its
    // weight.
    let need = |dim: Dim, amount: u64| if dim == Dim::Cpu { least } else { amount };
    let mut cpu_room = most;
    let mut causes: BTreeMap<Dim, Cause> = BTreeMap::new();
    let waits = |infos: &[Info]| (waiting(infos, at, Vec::new()), 0);

    // 1. Now: the running jobs ahead, those of this pass included, and the
    // measure-only ones.
    let now: Vec<usize> = (0..at)
        .filter(|&i| running[i])
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
        if used + need(dim, amount) > limit {
            if !explain {
                return waits(infos);
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
        } else if dim == Dim::Cpu {
            cpu_room = cpu_room.min(limit - used);
        }
    }

    // 2. Beside every start ahead. Where a dimension that is not tracked is
    // bounded by its sum alone, the most over the sets that fit is worked
    // out only once nothing else keeps the job waiting, or to explain.
    let mut unsure: Vec<(&Beside, Dim, u64, u64)> = Vec::new();
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
            // On CPU, worth a closer look while it could start with more.
            let wanted = if dim == Dim::Cpu { most } else { amount };
            let mut used = beside.heaviest(dim) + beside.x_amount(dim);
            if used + wanted > limit && beside.untracked(dim) {
                if !explain {
                    unsure.push((beside, dim, need(dim, amount), limit));
                    continue;
                }
                used = beside.used(infos, dim);
            }
            if used + need(dim, amount) > limit {
                if !explain {
                    return waits(infos);
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
            } else if dim == Dim::Cpu {
                cpu_room = cpu_room.min(limit - used);
            }
        }
    }
    for (beside, dim, needed, limit) in unsure {
        let used = beside.used(infos, dim);
        if used + needed > limit {
            return waits(infos);
        }
        if dim == Dim::Cpu {
            cpu_room = cpu_room.min(limit - used);
        }
    }

    if causes.is_empty() {
        return (Admission::Admitted, cpu_room);
    }
    let short = k
        .now
        .iter()
        .filter_map(|&(dim, amount, _)| {
            Some(Shortage {
                need: dim.need(),
                amount: need(dim, amount),
                cause: causes.remove(&dim)?,
            })
        })
        .collect();
    (waiting(infos, at, short), 0)
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
