//! Scheduling policies and the driver loop that turns a `Session` plus a
//! policy into a recorded `Trace`.

use anyhow::{bail, Result};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

use crate::session::{Event, Session};
use crate::task::Ltid;
use crate::trace::{describe, Outcome, Step, Trace};

/// What the driver should do next.
#[derive(Clone, Debug)]
pub enum Decision {
    /// Run `ltid` until its next event, preempting after `budget` ticks
    /// (0 = never) and then single-stepping `steps` more instructions, so
    /// that a preemption can land anywhere, not only at a conditional
    /// branch.
    Run { ltid: Ltid, budget: u64, steps: u32 },
    /// Run `ltid` exactly to the point (ticks, rip); if `signal` is set,
    /// make that signal (number, siginfo hex) arrive there.
    RunTo {
        ltid: Ltid,
        ticks: u64,
        rip: u64,
        signal: Option<(i32, String)>,
        /// The recorded event is a slice-cap preemption (`Event::Slice`).
        slice: bool,
    },
    /// Resume `ltid` into its syscall and consider it blocked.
    ResumeBlocked { ltid: Ltid },
    /// Replay: run `ltid` through its syscall with ffs's preemption signal
    /// pending, as it was in the recording (the trace shows the preemption
    /// right after the syscall exit, at the same ticks and instruction).
    /// The kernel cuts some results short when a signal is pending:
    /// `getdents64` returns one entry, a pipe write stops between chunks.
    RunPending { ltid: Ltid },
    /// Normalize all threads as a checkpoint would (replay of a `Fork` step).
    Fork,
    /// Stop the run with this outcome.
    Stop(Outcome),
}

#[allow(dead_code)]
pub struct SchedContext<'a> {
    pub current: Option<Ltid>,
    pub last_event: Option<&'a Event>,
    pub enabled: &'a [Ltid],
    pub blocked: &'a [Ltid],
    pub step: usize,
    /// The last event was a preemption caused by a watchpoint (an access
    /// to a shared variable), not by the expiry of a tick budget.
    pub watch_preempt: bool,
    /// Spin level of `current` (see `SpinTracker`): 0 unless its last few
    /// slices all ended by tick budget in the same code page.
    pub spin_level: u32,
    /// Threads that were blocked at the previous decision and are enabled
    /// now: woken by the last step (or by a virtual timer).
    pub woken: &'a [Ltid],
    /// Enabled threads currently classified as spinning (see `SpinTracker`).
    pub spinning: &'a [Ltid],
}

/// Busy-wait detection.  Under a serializing scheduler a thread that spins
/// on a condition another thread must produce (`while (!done) {}`, a
/// polling loop) runs its whole slice every time it is scheduled, where
/// natively the other thread would have satisfied it in microseconds on
/// another core.  That inflates virtual time (a 1M-tick slice is a
/// millisecond), wall time, and under PCT it starves every lower-priority
/// thread forever.  A thread whose consecutive slices all end by tick
/// budget with the instruction pointer in the same 4 KiB page is taken to
/// be spinning: its slices shrink by 4x per level (down to
/// `SPIN_MIN_SLICE`), and PCT/FIFO treat its preemptions as yields.  A
/// long computation moves through many pages and is left alone; a hot
/// numeric loop looks like a spinner and just gets more scheduling points.
#[derive(Default)]
pub struct SpinTracker {
    /// Per thread: page of the last preemption, consecutive same-page
    /// preemptions.
    per: std::collections::BTreeMap<Ltid, (u64, u32)>,
    /// Per thread: consecutive preemptions on any page.  A thread that
    /// retries an operation until another thread finishes its change (an
    /// optimistic descent restarting from the root while a writer holds a
    /// node) runs a loop spread over many pages; under strict priorities it
    /// never ends either.
    runs: std::collections::BTreeMap<Ltid, u32>,
}

/// Same-page preemptions before a thread counts as spinning.
const SPIN_STREAK: u32 = 2;
/// Smallest slice a spinner is given (ticks).
pub const SPIN_MIN_SLICE: u64 = 16 * 1024;


impl SpinTracker {
    pub fn observe(&mut self, step: &Step, watch_preempt: bool) {
        match step.event {
            Event::Preempt | Event::Slice if !watch_preempt => {
                let page = step.rip >> 12;
                let e = self.per.entry(step.ltid).or_insert((page, 0));
                // A loop body straddling a page boundary alternates
                // between two neighbouring pages.
                if e.0.abs_diff(page) <= 1 {
                    e.1 += 1;
                } else {
                    *e = (page, 1);
                }
                *self.runs.entry(step.ltid).or_insert(0) += 1;
            }
            Event::Fork | Event::SyscallEntry { .. } => {}
            _ => {
                // A polling loop still makes the odd syscall (a contended
                // lock inside it, a yield): that only weakens the evidence,
                // real work moves to other pages and resets it above.
                if let Some(e) = self.per.get_mut(&step.ltid) {
                    e.1 = e.1.saturating_sub(1);
                }
                if let Some(r) = self.runs.get_mut(&step.ltid) {
                    *r = r.saturating_sub(1);
                }
            }
        }
    }

    /// 0: the thread's last events were not all preemptions; n: it was
    /// preempted n + `SPIN_STREAK` times in a row, on any pages.  What a
    /// policy with strict priorities (PCT) uses to bound how long a thread
    /// runs while others could: a spinner or retry loop at the top priority
    /// otherwise starves the thread it waits for.
    pub fn run_level(&self, ltid: Ltid) -> u32 {
        self.runs.get(&ltid).map(|r| r.saturating_sub(SPIN_STREAK)).unwrap_or(0)
    }

    /// The threads among `enabled` that are spinning.
    pub fn spinning(&self, enabled: &[Ltid]) -> Vec<Ltid> {
        enabled.iter().copied().filter(|l| self.level(*l) > 0).collect()
    }

    /// 0: not spinning; n: spinning, n levels deep.
    pub fn level(&self, ltid: Ltid) -> u32 {
        self.per.get(&ltid).map(|e| e.1.saturating_sub(SPIN_STREAK)).unwrap_or(0)
    }

    /// The budget a spinning thread gets instead of `budget` (which must
    /// be nonzero: an unbounded run is left alone, as is a replay).
    pub fn cap(&self, ltid: Ltid, budget: u64) -> u64 {
        let level = self.level(ltid);
        if budget == 0 || level == 0 {
            return budget;
        }
        let cap = (budget >> (2 * level.min(10))).max(SPIN_MIN_SLICE);
        budget.min(cap)
    }
}

pub trait Policy {
    fn decide(&mut self, ctx: &SchedContext) -> Result<Decision>;
    /// Replay: the virtual time the next step was recorded at, if known.
    fn expected_vt(&self) -> Option<u64> {
        None
    }
    /// Called after each step; returns Err to abort the run (e.g. divergence).
    fn observe(&mut self, _step: &Step) -> Result<Option<Outcome>> {
        Ok(None)
    }
}

/// Keep running the current thread until it blocks or exits; while other
/// threads are runnable a slice is bounded by `max_slice` ticks (0 = not
/// at all), after which the next thread gets its turn, so that a thread
/// spinning on another one's progress cannot livelock the schedule.
pub struct Fifo {
    pub max_slice: u64,
    /// Consecutive events of the current thread.
    pub run: usize,
}

/// Events of one thread after which fifo moves on even without a
/// preemption (a thread spinning on syscalls, e.g. `sched_yield`).
const FIFO_MAX_RUN: usize = 10_000;

impl Fifo {
    pub fn new(max_slice: u64) -> Fifo {
        Fifo { max_slice, run: 0 }
    }
}

impl Policy for Fifo {
    fn decide(&mut self, ctx: &SchedContext) -> Result<Decision> {
        let rotate = matches!(ctx.last_event, Some(Event::Preempt | Event::Slice))
            || is_yield(ctx.last_event)
            || ctx.spin_level > 0
            || self.run >= FIFO_MAX_RUN;
        let ltid = match ctx.current {
            Some(c) if ctx.enabled.contains(&c) && !rotate => c,
            Some(c) => ctx
                .enabled
                .iter()
                .copied()
                .find(|&l| l > c)
                .unwrap_or(ctx.enabled[0]),
            _ => ctx.enabled[0],
        };
        self.run = if Some(ltid) == ctx.current { self.run + 1 } else { 0 };
        let budget = if others_may_run(ctx) { self.max_slice } else { 0 };
        Ok(Decision::Run { ltid, budget, steps: 0 })
    }
}

/// Should a slice be bounded?  Yes while any other thread exists that could
/// run: one that is enabled, or one blocked in the kernel, whose wakeup
/// (the lock the running thread just released, data it just wrote) may be
/// in flight and must not wait behind an unbounded run of a busy-waiting
/// thread until the real-time cap.
pub fn others_may_run(ctx: &SchedContext) -> bool {
    ctx.enabled.len() > 1 || !ctx.blocked.is_empty()
}

/// The last event was a `sched_yield` or `ffs_yield`: the thread asked to
/// be scheduled away.
pub fn is_yield(ev: Option<&Event>) -> bool {
    const SCHED_YIELD: u64 = 24;
    matches!(ev, Some(Event::SyscallExit { nr: SCHED_YIELD, .. }) | Some(Event::Yield))
}

/// Switch to the next enabled thread at every event; every slice is at
/// most `max_slice` ticks long.
pub struct RoundRobin {
    pub max_slice: u64,
}

impl Policy for RoundRobin {
    fn decide(&mut self, ctx: &SchedContext) -> Result<Decision> {
        let ltid = match ctx.current {
            Some(c) => ctx
                .enabled
                .iter()
                .copied()
                .find(|&l| l > c)
                .unwrap_or(ctx.enabled[0]),
            None => ctx.enabled[0],
        };
        let budget = if others_may_run(ctx) { self.max_slice } else { 0 };
        Ok(Decision::Run { ltid, budget, steps: 0 })
    }
}

/// Random scheduling: at every event pick a random enabled thread, and with
/// probability `preempt_prob` give it a random tick budget.
pub struct Random {
    pub rng: ChaCha8Rng,
    pub preempt_prob: f64,
    pub max_budget: u64,
    /// Probability of switching away from the current thread at an event.
    pub switch_prob: f64,
    /// Upper bound on any slice while other threads are runnable (0 = none).
    pub max_slice: u64,
    /// Instructions to single-step after a tick budget (uniform 0..=max).
    pub max_steps: u32,
    /// Probability of holding a thread back after a watchpoint hit.
    pub watch_hold: f64,
    hold: WatchHold,
    pub wake_delay: f64,
    wake: WakeDelay,
    pub spin_avoid: f64,
}

impl Random {
    pub fn new(
        seed: u64,
        preempt_prob: f64,
        max_budget: u64,
        switch_prob: f64,
        max_slice: u64,
        max_steps: u32,
    ) -> Random {
        Random {
            rng: ChaCha8Rng::seed_from_u64(seed),
            preempt_prob,
            max_budget,
            switch_prob,
            max_slice,
            max_steps,
            watch_hold: 0.0,
            hold: WatchHold::default(),
            wake_delay: 0.0,
            wake: WakeDelay::default(),
            spin_avoid: 0.0,
        }
    }
}

impl Policy for Random {
    fn decide(&mut self, ctx: &SchedContext) -> Result<Decision> {
        // A budget expiry or a yield always moves on (that is its point);
        // after an access to a watched variable the thread may well
        // continue, so that is a coin flip like any other event.
        let preempted = (matches!(ctx.last_event, Some(Event::Preempt | Event::Slice)) && !ctx.watch_preempt)
            || is_yield(ctx.last_event);
        let held = self.hold.update(ctx, &mut self.rng, self.watch_hold);
        self.wake.update(ctx, &mut self.rng, self.wake_delay);
        let enabled: Vec<Ltid> = self.wake.filter(ctx.enabled, ctx.step).into_iter().filter(|&l| Some(l) != held).collect();
        let enabled = if enabled.is_empty() { ctx.enabled.to_vec() } else { enabled };
        // A spinner waits for someone else: running it again is almost
        // never the step that makes progress, so most of the time the
        // choice is made among the others (it still gets its turn, which
        // a lock holder spinning on a second lock needs).
        let non_spinners: Vec<Ltid> = enabled.iter().copied().filter(|l| !ctx.spinning.contains(l)).collect();
        let enabled = if !non_spinners.is_empty() && non_spinners.len() < enabled.len() && self.spin_avoid > 0.0 && self.rng.gen_bool(self.spin_avoid) {
            non_spinners
        } else {
            enabled
        };
        let keep = match ctx.current {
            Some(c) if enabled.contains(&c) && !preempted => !self.rng.gen_bool(self.switch_prob),
            _ => false,
        };
        let ltid = if keep {
            ctx.current.unwrap()
        } else {
            // After a preemption always move to a different thread if there
            // is one: that is the whole point of preempting.
            let others: Vec<Ltid> = enabled.iter().copied().filter(|&l| Some(l) != ctx.current).collect();
            if (preempted || held.is_some()) && !others.is_empty() {
                others[self.rng.gen_range(0..others.len())]
            } else {
                enabled[self.rng.gen_range(0..enabled.len())]
            }
        };
        let mut budget = if self.max_budget > 0 && self.rng.gen_bool(self.preempt_prob) {
            log_uniform(&mut self.rng, self.max_budget)
        } else {
            0
        };
        let steps = if budget > 0 && self.max_steps > 0 { self.rng.gen_range(0..=self.max_steps) } else { 0 };
        if self.max_slice > 0 && others_may_run(ctx) && (budget == 0 || budget > self.max_slice) {
            budget = self.max_slice;
        }
        Ok(Decision::Run { ltid, budget, steps })
    }
}

/// PCT (Burckhardt et al., ASPLOS 2010): every thread gets a random
/// priority, the highest-priority runnable thread always runs, and at
/// `depth - 1` randomly chosen steps the running thread's priority drops
/// below everyone else's.  Finds any bug of depth d with probability at
/// least 1/(n * k^(d-1)) per run.  Steps are scheduling events; random tick
/// budgets (plus instruction steps) add preemption points inside user code
/// so that straight-line races are reachable too.
pub struct Pct {
    pub rng: ChaCha8Rng,
    pub depth: u32,
    pub preempt_prob: f64,
    pub max_budget: u64,
    pub max_slice: u64,
    pub max_steps: u32,
    /// Probability that a watchpoint hit acts as a priority change point.
    pub watch_hold: f64,
    pub wake_delay: f64,
    wake: WakeDelay,
    priorities: std::collections::BTreeMap<Ltid, i64>,
    change_points: Vec<usize>,
    step: usize,
    lowest: i64,
    /// Spin levels (`SpinTracker`) the running thread may reach before it
    /// counts as yielding.  A thread spinning on a latch a lower-priority
    /// thread holds must be demoted or nothing ever runs the holder; but a
    /// long numeric loop looks the same, and demoting it loses every
    /// schedule where it finishes before the others start.  Drawn
    /// log-uniformly per policy (per branch), so that some branches keep
    /// a looping thread and others move on quickly.
    spin_patience: u32,
    debug: bool,
}

impl Pct {
    /// `expected_steps` is the length (in events) of a typical run, used to
    /// place the change points.
    pub fn new(seed: u64, depth: u32, expected_steps: usize, preempt_prob: f64, max_budget: u64, max_slice: u64, max_steps: u32) -> Pct {
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let k = expected_steps.max(1);
        let mut change_points: Vec<usize> = (1..depth).map(|_| rng.gen_range(0..k)).collect();
        change_points.sort_unstable();
        let spin_patience = log_uniform(&mut rng, SPIN_PATIENCE_MAX) as u32;
        Pct {
            rng,
            depth,
            preempt_prob,
            max_budget,
            max_slice,
            max_steps,
            watch_hold: 0.0,
            wake_delay: 0.0,
            wake: WakeDelay::default(),
            priorities: Default::default(),
            change_points,
            step: 0,
            lowest: 0,
            spin_patience,
            debug: std::env::var_os("FFS_PCT_DEBUG").is_some(),
        }
    }
}

/// The most spin levels a PCT policy tolerates: a level is one preemption,
/// a slice of at most `--max-slice` ticks (shrunk under `sched::run`), so a
/// few million ticks of spinning at most before the spinner is demoted.
const SPIN_PATIENCE_MAX: u64 = 256;

impl Policy for Pct {
    fn decide(&mut self, ctx: &SchedContext) -> Result<Decision> {
        for &l in ctx.enabled {
            if !self.priorities.contains_key(&l) {
                // Initial priorities are positive; changed ones go below.
                let p: i64 = self.rng.gen_range(1..=1_000_000_000);
                self.priorities.insert(l, p);
            }
        }
        if let Some(c) = ctx.current {
            // A change point, or a yield: strict priorities would otherwise
            // let a thread spinning on `sched_yield` starve everyone else.
            // A watchpoint hit is also one with probability `watch_hold`:
            // the thread is inside a read-modify-write of shared state, and
            // dropping its priority lets another thread reach the
            // conflicting access first.
            let watch_hit = ctx.watch_preempt && self.watch_hold > 0.0 && self.rng.gen_bool(self.watch_hold);
            if self.change_points.contains(&self.step) || is_yield(ctx.last_event) || ctx.spin_level >= self.spin_patience || watch_hit {
                self.lowest -= 1;
                self.priorities.insert(c, self.lowest);
                if self.debug {
                    eprintln!(
                        "[pct] step {} T{c} demoted (change point {}, yield {}, spin {}/{}) enabled={:?} prios={:?}",
                        self.step,
                        self.change_points.contains(&self.step),
                        is_yield(ctx.last_event),
                        ctx.spin_level,
                        self.spin_patience,
                        ctx.enabled,
                        self.priorities
                    );
                }
            }
        }
        self.step += 1;
        self.wake.update(ctx, &mut self.rng, self.wake_delay);
        let eligible = self.wake.filter(ctx.enabled, ctx.step);
        let ltid = *eligible.iter().max_by_key(|l| self.priorities[l]).unwrap();
        let mut budget = if self.max_budget > 0 && self.rng.gen_bool(self.preempt_prob) {
            log_uniform(&mut self.rng, self.max_budget)
        } else {
            0
        };
        let steps = if budget > 0 && self.max_steps > 0 { self.rng.gen_range(0..=self.max_steps) } else { 0 };
        if self.max_slice > 0 && others_may_run(ctx) && (budget == 0 || budget > self.max_slice) {
            budget = self.max_slice;
        }
        let _ = self.depth;
        Ok(Decision::Run { ltid, budget, steps })
    }
}

/// A budget in `1..=max` distributed log-uniformly, so that short loops get
/// preempted as often as long ones (cf. rr's chaos mode).
pub fn log_uniform(rng: &mut ChaCha8Rng, max: u64) -> u64 {
    let hi = (max.max(2) as f64).log2();
    let e: f64 = rng.gen_range(0.0..hi);
    (2f64.powf(e) as u64).clamp(1, max)
}

/// Syscalls whose result the kernel cuts short when a signal is pending
/// (filldir stops after one entry; pipe writes, sendfile and the splice
/// family stop between chunks).  Only these are re-executed with the
/// recorded pending preemption signal (`Decision::RunPending`): for a wait
/// such as `poll` the signal would instead make it return EINTR when its
/// descriptor is not ready yet in the replay, and the recorded preemption
/// at the exit address is reached without any signal anyway.
fn signal_shortens(nr: u64) -> bool {
    matches!(nr, 1 | 18 | 20 | 40 | 78 | 217 | 275 | 276 | 278 | 296 | 326)
}

/// Follow a recorded trace, verifying every event.
pub struct Replay {
    pub steps: Vec<Step>,
    pub idx: usize,
    pub strict_ret: bool,
    pub verify_outputs: bool,
    /// With `verify_outputs`: also compare metadata buffers (stat family,
    /// statfs, sysinfo, uname), which carry incidental live state such as
    /// inode numbers and timestamps.
    pub verify_metadata: bool,
}

impl Replay {
    pub fn new(trace: &Trace) -> Replay {
        Replay {
            steps: trace.steps.clone(),
            idx: 0,
            strict_ret: true,
            verify_outputs: false,
            verify_metadata: true,
        }
    }

    /// Every step has been replayed, or only the exits of threads that are
    /// already gone remain (a process killed as a whole ends the run before
    /// each thread's exit is stepped through).
    pub fn finished(&self) -> bool {
        self.steps[self.idx..]
            .iter()
            .all(|s| matches!(s.event, Event::Exit | Event::Exited { .. }))
    }
}

impl Policy for Replay {
    fn expected_vt(&self) -> Option<u64> {
        self.steps.get(self.idx).map(|s| s.vt).filter(|v| *v > 0)
    }

    fn decide(&mut self, ctx: &SchedContext) -> Result<Decision> {
        let Some(step) = self.steps.get(self.idx) else {
            return Ok(Decision::Stop(Outcome::Bound));
        };
        if step.event != Event::Fork
            && !ctx.enabled.contains(&step.ltid)
            && !ctx.blocked.contains(&step.ltid)
        {
            return Ok(Decision::Stop(Outcome::Divergence {
                at: self.idx,
                expected: format!("T{} runnable ({})", step.ltid, describe(&step.event)),
                actual: format!("enabled {:?}, blocked {:?}", ctx.enabled, ctx.blocked),
            }));
        }
        // A thread that died without calling exit/exit_group and without
        // a recorded fatal signal was killed from outside (SIGKILL): kill
        // it at the recorded point.
        if step.event == Event::Exit {
            const EXIT: u64 = 60;
            const EXIT_GROUP: u64 = 231;
            // Ways a thread can die from inside the session: exit, a
            // fatal signal (recorded), a kill sent by a tracee, a seccomp
            // filter installed earlier.
            const KILL: u64 = 62;
            const TKILL: u64 = 200;
            const TGKILL: u64 = 234;
            const RT_SIGQUEUEINFO: u64 = 129;
            const RT_TGSIGQUEUEINFO: u64 = 297;
            const PIDFD_SEND_SIGNAL: u64 = 424;
            const SECCOMP: u64 = 317;
            const PRCTL: u64 = 157;
            let own = self.steps[..self.idx]
                .iter()
                .rev()
                .find(|s| s.ltid == step.ltid)
                .map(|s| &s.event);
            let natural = matches!(
                own,
                Some(Event::SyscallEntry { nr: EXIT | EXIT_GROUP })
                    | Some(Event::Signal { .. })
                    | Some(Event::Exit)
            ) || self.steps[..self.idx].iter().rev().take(64).any(|s| {
                matches!(
                    s.event,
                    Event::SyscallEntry {
                        nr: EXIT_GROUP | KILL | TKILL | TGKILL | RT_SIGQUEUEINFO | RT_TGSIGQUEUEINFO | PIDFD_SEND_SIGNAL
                    } | Event::Exit
                )
            }) || self.steps[..self.idx]
                .iter()
                .any(|s| matches!(s.event, Event::SyscallEntry { nr: SECCOMP | PRCTL }));
            if !natural {
                return Ok(Decision::RunTo {
                    ltid: step.ltid,
                    ticks: step.ticks,
                    rip: step.rip,
                    signal: Some((libc::SIGKILL, String::new())),
                    slice: false,
                });
            }
        }
        Ok(match &step.event {
            Event::Fork => Decision::Fork,
            Event::Blocked { .. } => Decision::ResumeBlocked { ltid: step.ltid },
            Event::Preempt | Event::Slice => Decision::RunTo {
                ltid: step.ltid,
                ticks: step.ticks,
                rip: step.rip,
                signal: None,
                slice: step.event == Event::Slice,
            },
            Event::Signal { sig, info } => Decision::RunTo {
                ltid: step.ltid,
                ticks: step.ticks,
                rip: step.rip,
                signal: Some((*sig, info.clone())),
                slice: false,
            },
            Event::SyscallExit { nr, .. }
                if signal_shortens(*nr) && self.steps.get(self.idx + 1).is_some_and(|next| {
                    next.event == Event::Preempt
                        && next.ltid == step.ltid
                        && next.ticks == step.ticks
                        && next.rip == step.rip
                }) =>
            {
                Decision::RunPending { ltid: step.ltid }
            }
            _ => Decision::Run {
                ltid: step.ltid,
                budget: 0,
                steps: 0,
            },
        })
    }

    fn observe(&mut self, step: &Step) -> Result<Option<Outcome>> {
        let expected = &self.steps[self.idx];
        let same = match (&expected.event, &step.event) {
            (
                Event::SyscallExit {
                    nr: a,
                    child: ca,
                    ret: ra,
                },
                Event::SyscallExit {
                    nr: b,
                    child: cb,
                    ret: rb,
                },
            ) => a == b && ca == cb && (!self.strict_ret || ra == rb),
            (Event::Preempt, Event::Preempt) | (Event::Slice, Event::Slice) => {
                expected.ticks == step.ticks && expected.rip == step.rip
            }
            (Event::Signal { sig: a, .. }, Event::Signal { sig: b, .. }) => {
                a == b && expected.ticks == step.ticks && expected.rip == step.rip
            }
            (a, b) => a == b,
        };
        let metadata = matches!(step.event, Event::SyscallExit { nr, .. } if crate::effects::metadata_query(nr));
        let effects_match = !self.verify_outputs
            || (metadata && !self.verify_metadata)
            || expected.effects.as_ref().is_none_or(|e| step.effects.as_ref() == Some(e));
        if !same || expected.ltid != step.ltid || !effects_match {
            return Ok(Some(Outcome::Divergence {
                at: self.idx,
                expected: format!(
                    "T{} {} @{}:{:#x}",
                    expected.ltid,
                    describe(&expected.event),
                    expected.ticks,
                    expected.rip
                ),
                actual: format!(
                    "T{} {} @{}:{:#x}{}",
                    step.ltid,
                    describe(&step.event),
                    step.ticks,
                    step.rip,
                    if effects_match { "" } else { " (syscall output bytes or addresses differ)" }
                ),
            }));
        }
        self.idx += 1;
        Ok(None)
    }
}

/// Options for a driver run.
#[derive(Clone, Debug)]
pub struct RunOpts {
    pub max_steps: usize,
    /// Stop (successfully) at `ffs_end()`.
    pub stop_at_end: bool,
    pub verbose: bool,
}

impl Default for RunOpts {
    fn default() -> Self {
        RunOpts {
            max_steps: 10_000_000,
            stop_at_end: false,
            verbose: false,
        }
    }
}

/// Hook invoked for every step; may stop the run by returning an outcome.
pub type StepHook<'a> = &'a mut dyn FnMut(&mut Session, &Step) -> Result<Option<Outcome>>;

/// Settle the session and check for termination.  Returns an outcome if
/// the execution is over (all exited / deadlock), otherwise the enabled set.
pub fn observe(session: &mut Session, expected_vt: Option<u64>) -> Result<std::result::Result<Vec<Ltid>, Outcome>> {
    session.settle()?;
    session.wait_wakes()?;
    if session.all_exited() {
        let code = session.main_exit_code().unwrap_or(0);
        return Ok(Err(match code {
            c if c >= 128 => Outcome::Killed { sig: c - 128 },
            c => Outcome::Exited { code: c },
        }));
    }
    // The main process is gone: the run is over with its status (rr does
    // the same unless asked to wait for everything); what is left, daemons
    // and the like, is killed by the caller.
    if session.main_process_exited() {
        let code = session.main_exit_code().unwrap_or(0);
        return Ok(Err(match code {
            c if c >= 128 => Outcome::Killed { sig: c - 128 },
            c => Outcome::Exited { code: c },
        }));
    }
    if session.main_exit_code().is_none() {
        if let Some((vtime_ns, runnable, blocked)) = session.hang_check() {
            return Ok(Err(Outcome::Hang { vtime_ns, runnable, blocked }));
        }
    }
    let mut enabled = session.enabled();
    if enabled.is_empty() {
        if let Some(d) = session.deadlock_check() {
            return Ok(Err(Outcome::Deadlock {
                blocked: d.blocked,
                timers: d.timers,
                vtime_ns: d.vtime_ns,
                wakes: d.wakes,
            }));
        }
        if session.replay && expected_vt.is_some_and(|vt| vt > session.virtual_ns()) {
            // The trace advances the clock before its next step (the
            // recording jumped to a deadline here): nothing to wait for.
            return Ok(Ok(enabled));
        }
        let progressed = if session.replay {
            // A replay never jumps the clock on its own (the trace says
            // when); it waits for the real completion the trace promises.
            session.wait_for_completion()?
        } else {
            session.wait_for_any_enabled()?
        };
        if !progressed {
            return Ok(Err(Outcome::Deadlock {
                blocked: session.blocked(),
                timers: Vec::new(),
                vtime_ns: 0,
                wakes: 0,
            }));
        }
        if session.all_exited() {
            return observe(session, expected_vt);
        }
        enabled = session.enabled();
    }
    Ok(Ok(enabled))
}

/// Carry out a decision and return the resulting step.
pub fn perform(session: &mut Session, decision: &Decision) -> Result<Step> {
    let (ltid, event) = match decision {
        Decision::Run { ltid, budget, steps } => {
            let mut ev = if session.opts.exact_preempt && *budget > 0 && !session.replay {
                session.run_task_exact(*ltid, *budget)?
            } else {
                session.run_task(*ltid, *budget)?
            };
            if ev == Event::Preempt && *steps > 0 && !session.watch_preempt {
                ev = session.step_instructions(*ltid, *steps)?;
            }
            (*ltid, ev)
        }
        Decision::RunTo { ltid, ticks, rip, signal, slice } => {
            let mut ev = session.run_task_to(*ltid, *ticks, *rip, signal.as_ref())?;
            if ev == Event::Preempt && *slice {
                // A slice-cap preemption: the clock advanced by the cap.
                session.vclock.skew += session.opts.slice_cap.as_nanos() as u64;
                ev = Event::Slice;
            }
            (*ltid, ev)
        }
        Decision::ResumeBlocked { ltid } => (*ltid, session.resume_as_blocked(*ltid)?),
        Decision::RunPending { ltid } => {
            session.prearm_preempt(*ltid)?;
            (*ltid, session.run_task(*ltid, 0)?)
        }
        Decision::Fork => {
            session.normalize_all()?;
            return Ok(Step {
                ltid: 0,
                event: Event::Fork,
                ticks: 0,
                rip: 0,
                effects: None,
                vt: 0,
            });
        }
        Decision::Stop(_) => unreachable!(),
    };
    let (ticks, rip) = {
        let t = session.by_ltid(ltid)?;
        let rip = if t.is_stopped() { t.rip() } else { 0 };
        (t.ticks, rip)
    };
    let effects = crate::effects::capture(session, ltid, &event)?;
    Ok(Step {
        vt: 0,
        ltid,
        event,
        ticks,
        rip,
        effects,
    })
}

/// Outcome implied by an event, if it ends the execution.
pub fn event_outcome(event: &Event, opts: &RunOpts) -> Option<Outcome> {
    match event {
        Event::AssertFailed { msg } => Some(Outcome::AssertFailed { msg: msg.clone() }),
        Event::End if opts.stop_at_end => Some(Outcome::End),
        _ => None,
    }
}

/// Drive `session` with `policy`, appending steps to `trace`, until an
/// outcome is reached.
pub fn drive(
    session: &mut Session,
    policy: &mut dyn Policy,
    trace: &mut Trace,
    opts: &RunOpts,
    mut hook: Option<StepHook>,
) -> Result<Outcome> {
    let mut current: Option<Ltid> = None;
    let mut last_event: Option<Event> = None;
    let mut steps = 0usize;
    let mut spin = SpinTracker::default();
    let mut prev_blocked: Vec<Ltid> = Vec::new();
    let finish = |trace: &mut Trace, outcome: Outcome| {
        trace.outcome = outcome.clone();
        Ok(outcome)
    };
    loop {
        if crate::session::TERMINATED.load(std::sync::atomic::Ordering::SeqCst) {
            return finish(trace, Outcome::Error { msg: "terminated by SIGTERM".into() });
        }
        let expected_vt = policy.expected_vt();
        let mut enabled = match observe(session, expected_vt)? {
            Ok(e) => e,
            Err(outcome) => return finish(trace, outcome),
        };
        if steps >= opts.max_steps {
            return finish(trace, Outcome::Bound);
        }
        if let Some(vt) = expected_vt {
            // Replay: put the virtual clock where the recording had it.
            let now = session.virtual_ns();
            if vt > now {
                session.vclock.skew += vt - now;
                // Deadlines reached by the jump wake their threads.
                session.vt_check()?;
                enabled = session.enabled();
            } else if vt < now {
                log::debug!("replay clock ahead of the trace at step {}: {} > {}", trace.steps.len(), now, vt);
            }
        }
        let vt_before = session.virtual_ns();
        let blocked = session.blocked();
        let woken: Vec<Ltid> = enabled.iter().copied().filter(|l| prev_blocked.contains(l)).collect();
        prev_blocked = blocked.clone();
        let spinning = spin.spinning(&enabled);
        if opts.verbose {
            eprintln!("[ffs]       enabled={enabled:?} blocked={blocked:?} vt={}", session.virtual_ns());
        }
        let mut decision = policy.decide(&SchedContext {
            current,
            last_event: last_event.as_ref(),
            enabled: &enabled,
            blocked: &blocked,
            step: trace.steps.len(),
            watch_preempt: session.watch_preempt,
            spin_level: current.map(|c| spin.level(c)).unwrap_or(0),
            woken: &woken,
            spinning: &spinning,
        })?;
        if let Decision::Stop(outcome) = decision {
            return finish(trace, outcome);
        }
        if let Decision::Run { ltid, budget, steps: insn } = &mut decision {
            // A spinner alone (the others blocked, perhaps with a wakeup in
            // flight) keeps the ordinary bounded slice: shrinking it would
            // only multiply the steps spent waiting for them.
            let capped = if enabled.len() > 1 { spin.cap(*ltid, *budget) } else { *budget };
            if capped != *budget {
                *budget = capped;
                *insn = 0;
            }
        }
        let mut step = perform(session, &decision)?;
        step.vt = vt_before;
        spin.observe(&step, session.watch_preempt);
        if matches!(step.event, Event::Exit | Event::Exited { .. }) {
            // A thread's exit wakes its joiners (CLONE_CHILD_CLEARTID).
            session.wake_pending = true;
        }
        if opts.verbose {
            eprintln!(
                "[ffs] {:5} T{} ticks={} rip={:#x} vt={} {}",
                trace.steps.len(),
                step.ltid,
                step.ticks,
                step.rip,
                session.virtual_ns(),
                describe(&step.event)
            );
        }
        trace.steps.push(step.clone());
        steps += 1;
        if step.event != Event::Fork {
            current = Some(step.ltid);
        }
        if let Some(outcome) = policy.observe(&step)? {
            return finish(trace, outcome);
        }
        if let Some(h) = hook.as_mut() {
            if let Some(outcome) = h(session, &step)? {
                return finish(trace, outcome);
            }
        }
        if let Some(outcome) = event_outcome(&step.event, opts) {
            return finish(trace, outcome);
        }
        last_event = Some(step.event);
    }
}

/// Policy parameters shared by the commands.
#[derive(Clone, Debug)]
pub struct PolicyParams {
    pub name: String,
    pub seed: u64,
    pub preempt_prob: f64,
    pub max_budget: u64,
    pub switch_prob: f64,
    pub max_slice: u64,
    pub max_steps: u32,
    pub pct_depth: u32,
    /// Explicit watchpoints (see `watch.rs`).
    pub watch: Vec<String>,
    /// Globals sampled per branch when `watch` is empty.
    pub watch_count: u32,
    /// Probability of holding a thread back after a watchpoint hit.
    pub watch_hold: f64,
    /// Probability of delaying a just-woken thread (see `WakeDelay`).
    pub wake_delay: f64,
    /// Probability that the random policy chooses among the non-spinning
    /// threads when there are any.
    pub spin_avoid: f64,
}

/// Events a thread held back after a watchpoint hit stays unscheduled.
const WATCH_HOLD_EVENTS: usize = 64;

/// Active-testing style delay (cf. RaceFuzzer): a thread stopped by a
/// watchpoint is inside a read-modify-write of shared state, so holding it
/// there lets another thread reach the conflicting access.  The hold ends
/// when another thread hits a watchpoint (the racing pair is formed; the
/// policy's normal choice then decides who goes first), after
/// `WATCH_HOLD_EVENTS`, or when nothing else can run.
#[derive(Default)]
struct WatchHold {
    held: Option<(Ltid, usize)>,
}

impl WatchHold {
    /// Update on an event; returns the thread to keep off the enabled set.
    fn update(&mut self, ctx: &SchedContext, rng: &mut ChaCha8Rng, prob: f64) -> Option<Ltid> {
        if let Some((h, left)) = self.held {
            let formed = ctx.watch_preempt && ctx.current != Some(h);
            let alone = ctx.enabled.iter().all(|&l| l == h);
            if formed || alone || left == 0 || !ctx.enabled.contains(&h) {
                self.held = None;
            } else {
                self.held = Some((h, left - 1));
            }
        }
        if self.held.is_none() && prob > 0.0 && ctx.watch_preempt {
            if let Some(c) = ctx.current {
                if ctx.enabled.iter().any(|&l| l != c) && rng.gen_bool(prob) {
                    self.held = Some((c, WATCH_HOLD_EVENTS));
                }
            }
        }
        self.held.map(|(h, _)| h)
    }
}

/// Longest delay (events) of a just-woken thread.
const WAKE_DELAY_MAX: u64 = 64;

/// Natively a thread woken by another needs tens of microseconds to get a
/// CPU, and the waker keeps running meanwhile; a serializing scheduler that
/// picks the woken thread at once never explores that window (tokio's
/// blocking-pool shutdown race, #7922, fails natively every time and under
/// `random` in one schedule of seven).  With probability `prob` a woken
/// thread is kept off the enabled set for a log-uniform number of events.
#[derive(Default)]
struct WakeDelay {
    until: std::collections::BTreeMap<Ltid, usize>,
}

impl WakeDelay {
    fn update(&mut self, ctx: &SchedContext, rng: &mut ChaCha8Rng, prob: f64) {
        self.until.retain(|_, u| *u > ctx.step);
        if prob <= 0.0 {
            return;
        }
        for &w in ctx.woken {
            if Some(w) != ctx.current && !self.until.contains_key(&w) && rng.gen_bool(prob) {
                let d = log_uniform(rng, WAKE_DELAY_MAX).max(1) as usize;
                self.until.insert(w, ctx.step + d);
            }
        }
    }

    /// The enabled set without the delayed threads (all of it if nothing
    /// else could run).
    fn filter(&self, enabled: &[Ltid], step: usize) -> Vec<Ltid> {
        let v: Vec<Ltid> = enabled.iter().copied().filter(|l| self.until.get(l).is_none_or(|u| *u <= step)).collect();
        if v.is_empty() { enabled.to_vec() } else { v }
    }
}

/// `branch` selects the member of a policy portfolio (`mix` alternates
/// random and PCT, which find different bugs).
pub fn make_policy(p: &PolicyParams, seed: u64, expected_steps: usize) -> Result<Box<dyn Policy>> {
    make_branch_policy(p, seed, expected_steps, 0)
}

pub fn make_branch_policy(p: &PolicyParams, seed: u64, expected_steps: usize, branch: usize) -> Result<Box<dyn Policy>> {
    let name = match p.name.as_str() {
        "mix" => {
            if branch.is_multiple_of(2) {
                "random"
            } else {
                "pct"
            }
        }
        n => n,
    };
    // The wake delay helps wakeup-latency races and hurts others (a long
    // computation that must stay ahead of a thread it just woke), so like
    // the policy it alternates: branches 0,1 of every four use it.
    let wake_delay = if (branch >> 1).is_multiple_of(2) { p.wake_delay } else { 0.0 };
    Ok(match name {
        "fifo" => Box::new(Fifo::new(p.max_slice)),
        "rr" | "round-robin" => Box::new(RoundRobin { max_slice: p.max_slice }),
        "random" => {
            let mut r = Random::new(seed, p.preempt_prob, p.max_budget, p.switch_prob, p.max_slice, p.max_steps);
            r.watch_hold = p.watch_hold;
            r.wake_delay = wake_delay;
            r.spin_avoid = p.spin_avoid;
            Box::new(r)
        }
        "pct" => {
            let mut r = Pct::new(seed, p.pct_depth, expected_steps, p.preempt_prob, p.max_budget, p.max_slice, p.max_steps);
            r.watch_hold = p.watch_hold;
            r.wake_delay = wake_delay;
            Box::new(r)
        }
        other => bail!("unknown policy {other}"),
    })
}
