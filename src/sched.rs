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
}

pub trait Policy {
    fn decide(&mut self, ctx: &SchedContext) -> Result<Decision>;
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
        let budget = if ctx.enabled.len() > 1 { self.max_slice } else { 0 };
        Ok(Decision::Run { ltid, budget, steps: 0 })
    }
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
        let budget = if ctx.enabled.len() > 1 {
            self.max_slice
        } else {
            0
        };
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
        let keep = match ctx.current {
            Some(c) if ctx.enabled.contains(&c) && !preempted => {
                !self.rng.gen_bool(self.switch_prob)
            }
            _ => false,
        };
        let ltid = if keep {
            ctx.current.unwrap()
        } else {
            // After a preemption always move to a different thread if there
            // is one: that is the whole point of preempting.
            let others: Vec<Ltid> = ctx
                .enabled
                .iter()
                .copied()
                .filter(|&l| Some(l) != ctx.current)
                .collect();
            if preempted && !others.is_empty() {
                others[self.rng.gen_range(0..others.len())]
            } else {
                ctx.enabled[self.rng.gen_range(0..ctx.enabled.len())]
            }
        };
        let mut budget = if self.max_budget > 0 && self.rng.gen_bool(self.preempt_prob) {
            log_uniform(&mut self.rng, self.max_budget)
        } else {
            0
        };
        let steps = if budget > 0 && self.max_steps > 0 { self.rng.gen_range(0..=self.max_steps) } else { 0 };
        if self.max_slice > 0 && ctx.enabled.len() > 1 && (budget == 0 || budget > self.max_slice) {
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
    priorities: std::collections::BTreeMap<Ltid, i64>,
    change_points: Vec<usize>,
    step: usize,
    lowest: i64,
}

impl Pct {
    /// `expected_steps` is the length (in events) of a typical run, used to
    /// place the change points.
    pub fn new(seed: u64, depth: u32, expected_steps: usize, preempt_prob: f64, max_budget: u64, max_slice: u64, max_steps: u32) -> Pct {
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let k = expected_steps.max(1);
        let mut change_points: Vec<usize> = (1..depth).map(|_| rng.gen_range(0..k)).collect();
        change_points.sort_unstable();
        Pct {
            rng,
            depth,
            preempt_prob,
            max_budget,
            max_slice,
            max_steps,
            priorities: Default::default(),
            change_points,
            step: 0,
            lowest: 0,
        }
    }
}

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
            if self.change_points.contains(&self.step) || is_yield(ctx.last_event) {
                self.lowest -= 1;
                self.priorities.insert(c, self.lowest);
            }
        }
        self.step += 1;
        let ltid = *ctx
            .enabled
            .iter()
            .max_by_key(|l| self.priorities[l])
            .unwrap();
        let mut budget = if self.max_budget > 0 && self.rng.gen_bool(self.preempt_prob) {
            log_uniform(&mut self.rng, self.max_budget)
        } else {
            0
        };
        let steps = if budget > 0 && self.max_steps > 0 { self.rng.gen_range(0..=self.max_steps) } else { 0 };
        if self.max_slice > 0 && ctx.enabled.len() > 1 && (budget == 0 || budget > self.max_slice) {
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

/// Follow a recorded trace, verifying every event.
pub struct Replay {
    pub steps: Vec<Step>,
    pub idx: usize,
    pub strict_ret: bool,
}

impl Replay {
    pub fn new(trace: &Trace) -> Replay {
        Replay {
            steps: trace.steps.clone(),
            idx: 0,
            strict_ret: false,
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
        if !same || expected.ltid != step.ltid {
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
                    "T{} {} @{}:{:#x}",
                    step.ltid,
                    describe(&step.event),
                    step.ticks,
                    step.rip
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
pub fn observe(session: &mut Session) -> Result<std::result::Result<Vec<Ltid>, Outcome>> {
    session.settle()?;
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
    let mut enabled = session.enabled();
    if enabled.is_empty() {
        if !session.wait_for_any_enabled()? {
            return Ok(Err(Outcome::Deadlock {
                blocked: session.blocked(),
            }));
        }
        if session.all_exited() {
            return observe(session);
        }
        enabled = session.enabled();
    }
    Ok(Ok(enabled))
}

/// Carry out a decision and return the resulting step.
pub fn perform(session: &mut Session, decision: &Decision) -> Result<Step> {
    let (ltid, event) = match decision {
        Decision::Run { ltid, budget, steps } => {
            let mut ev = session.run_task(*ltid, *budget)?;
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
        Decision::Fork => {
            session.normalize_all()?;
            return Ok(Step {
                ltid: 0,
                event: Event::Fork,
                ticks: 0,
                rip: 0,
            });
        }
        Decision::Stop(_) => unreachable!(),
    };
    let (ticks, rip) = {
        let t = session.by_ltid(ltid)?;
        let rip = if t.is_stopped() { t.rip() } else { 0 };
        (t.ticks, rip)
    };
    Ok(Step {
        ltid,
        event,
        ticks,
        rip,
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
    let finish = |trace: &mut Trace, outcome: Outcome| {
        trace.outcome = outcome.clone();
        Ok(outcome)
    };
    loop {
        if crate::session::TERMINATED.load(std::sync::atomic::Ordering::SeqCst) {
            return finish(trace, Outcome::Error { msg: "terminated by SIGTERM".into() });
        }
        let enabled = match observe(session)? {
            Ok(e) => e,
            Err(outcome) => return finish(trace, outcome),
        };
        if steps >= opts.max_steps {
            return finish(trace, Outcome::Bound);
        }
        let blocked = session.blocked();
        let decision = policy.decide(&SchedContext {
            current,
            last_event: last_event.as_ref(),
            enabled: &enabled,
            blocked: &blocked,
            step: trace.steps.len(),
            watch_preempt: session.watch_preempt,
        })?;
        if let Decision::Stop(outcome) = decision {
            return finish(trace, outcome);
        }
        let step = perform(session, &decision)?;
        if opts.verbose {
            eprintln!(
                "[ffs] {:5} T{} ticks={} rip={:#x} {}",
                trace.steps.len(),
                step.ltid,
                step.ticks,
                step.rip,
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
    Ok(match name {
        "fifo" => Box::new(Fifo::new(p.max_slice)),
        "rr" | "round-robin" => Box::new(RoundRobin { max_slice: p.max_slice }),
        "random" => Box::new(Random::new(
            seed,
            p.preempt_prob,
            p.max_budget,
            p.switch_prob,
            p.max_slice,
            p.max_steps,
        )),
        "pct" => Box::new(Pct::new(seed, p.pct_depth, expected_steps, p.preempt_prob, p.max_budget, p.max_slice, p.max_steps)),
        other => bail!("unknown policy {other}"),
    })
}
