//! Schedule exploration from a checkpoint.
//!
//! The program is first run (deterministically) up to the exploration root:
//! the first `ffs_checkpoint()` / `ffs_begin()` call, or the start of the
//! program.  The root state is checkpointed by forking the tracee.  Every
//! branch then starts from a fresh fork of the root checkpoint, so the
//! (possibly expensive) bootstrap phase is executed only once.
//!
//! Two strategies are available:
//!
//! * `random`: each iteration is an independent random walk from the root
//!   (random thread choice at every event, random tick budgets).
//! * `dfs`: depth-first search over the tree of thread choices.  Every node
//!   with unexplored alternatives keeps a forked checkpoint, so backtracking
//!   is a fork rather than a re-execution; when the checkpoint budget is
//!   exhausted the path is re-executed from the nearest checkpoint instead.
//!
//! A failing branch is saved as a trace (bootstrap prefix + branch steps) and
//! immediately re-executed from the root checkpoint to confirm that it is
//! reproducible.

use anyhow::{anyhow, Context, Result};
use clap::Args;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use std::path::PathBuf;
use std::time::Instant;

use crate::perf::PmuConfig;
use crate::sched::{
    self, event_outcome, log_uniform, observe, perform, Decision, Policy, Replay, RunOpts,
    SchedContext,
};
use crate::session::{Event, Session, SessionOpts};
use crate::task::{Ltid, State, Stop};
use crate::trace::{describe, Outcome, Step, Trace};

/// Slice bound (ticks) for the fixed-schedule phases: reaching the root
/// and draining after `ffs_end()`.
const DRAIN_SLICE: u64 = 1_000_000;

#[derive(Args, Clone, Debug)]
pub struct ExploreOpts {
    /// Maximum number of branches (schedules) to execute.
    #[arg(long, default_value_t = 100)]
    pub iterations: usize,
    /// Exploration strategy: random, dfs.
    #[arg(long, default_value = "random")]
    pub strategy: String,
    /// Where exploration starts: auto (first ffs_checkpoint/ffs_begin, else
    /// start), checkpoint, begin, start.
    #[arg(long, default_value = "auto")]
    pub root: String,
    /// Directory where failing schedules are written.
    #[arg(long, default_value = "ffs-out")]
    pub output_dir: PathBuf,
    /// Maximum scheduling steps per branch after the root.
    #[arg(long, default_value_t = 100_000)]
    pub max_depth: usize,
    /// Maximum number of live checkpoints kept for backtracking (dfs).
    #[arg(long, default_value_t = 32)]
    pub max_checkpoints: usize,
    /// Keep exploring after the first failure.
    #[arg(long)]
    pub keep_going: bool,
    /// Do not verify failures by re-executing them.
    #[arg(long)]
    pub no_verify: bool,
}

pub use crate::sched::PolicyParams;

/// Threads that must be run before anything else (their state cannot be
/// cloned: about to exit, or with a signal to deliver).
fn forced_choices(session: &mut Session, enabled: &[Ltid]) -> Vec<Ltid> {
    enabled
        .iter()
        .copied()
        .filter(|&l| {
            let Ok(t) = session.by_ltid(l) else {
                return false;
            };
            matches!(t.state, State::Stopped(Stop::ExitEvent))
                || matches!(t.state, State::Stopped(Stop::Signal { .. }) if t.pending_sig != 0)
        })
        .collect()
}

/// Run `session` from its current state with `policy` until an outcome.
fn run_branch(
    session: &mut Session,
    policy: &mut dyn Policy,
    ropts: &RunOpts,
    max_depth: usize,
    steps: &mut Vec<Step>,
) -> Result<Outcome> {
    let mut current: Option<Ltid> = None;
    let mut n = 0usize;
    let mut drain = sched::Fifo::new(DRAIN_SLICE);
    let mut draining = false;
    loop {
        if crate::session::TERMINATED.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(Outcome::Error { msg: "terminated by SIGTERM".into() });
        }
        let expected_vt = policy.expected_vt();
        let mut enabled = match observe(session, expected_vt)? {
            Ok(e) => e,
            Err(o) => return Ok(o),
        };
        if let Some(vt) = expected_vt {
            let now = session.virtual_ns();
            if vt > now {
                session.vclock.skew += vt - now;
                session.vt_check()?;
                enabled = session.enabled();
            }
        }
        let vt_before = session.virtual_ns();
        if n >= max_depth {
            return Ok(Outcome::Bound);
        }
        let blocked = session.blocked();
        let policy: &mut dyn Policy = if draining { &mut drain } else { &mut *policy };
        let decision = policy.decide(&SchedContext {
            current,
            last_event: steps.last().map(|s| &s.event),
            enabled: &enabled,
            blocked: &blocked,
            step: steps.len(),
            watch_preempt: session.watch_preempt,
            spin_level: 0,
        })?;
        if let Decision::Stop(o) = decision {
            return Ok(o);
        }
        let mut step = perform(session, &decision)?;
        step.vt = vt_before;
        if ropts.verbose {
            eprintln!(
                "[ffs]   T{} ticks={} rip={:#x} {}",
                step.ltid,
                step.ticks,
                step.rip,
                describe(&step.event)
            );
        }
        if let Some(o) = policy.observe(&step)? {
            return Ok(o);
        }
        let ev = step.event.clone();
        current = Some(step.ltid);
        steps.push(step);
        n += 1;
        if ev == Event::End {
            // End of the region of interest: finish the program with a fixed
            // schedule (no more branching) so that final checks still run.
            draining = true;
            continue;
        }
        if let Some(o) = event_outcome(&ev, ropts) {
            return Ok(o);
        }
    }
}

struct Explorer {
    argv: Vec<String>,
    pmu: PmuConfig,
    ropts: RunOpts,
    params: PolicyParams,
    eopts: ExploreOpts,
    /// Trace of the bootstrap phase up to (and including) the root Fork step.
    prefix: Trace,
    root: Session,
    rng: ChaCha8Rng,
    failures: usize,
    branches: usize,
    /// Length of the previous branch (hint for PCT's change points).
    last_len: usize,
    /// Writable globals of the executable, candidates for watchpoints.
    globals: Vec<crate::watch::Global>,
    /// Explicit watchpoints (from `--watch`), if any.
    fixed_watch: Vec<u64>,
}

impl Explorer {
    fn save_failure(&mut self, steps: &[Step], outcome: &Outcome, ports: &[u16]) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.eopts.output_dir)?;
        let mut t = self.prefix.clone();
        t.steps.extend_from_slice(steps);
        t.outcome = outcome.clone();
        t.ports = ports.to_vec();
        self.failures += 1;
        let path = self
            .eopts
            .output_dir
            .join(format!("fail-{}.json", self.failures));
        t.save(&path)?;
        Ok(path)
    }

    /// Re-execute a branch from the root checkpoint and check the outcome.
    fn verify(&mut self, steps: &[Step], expected: &Outcome, ports: &[u16]) -> Result<bool> {
        let mut session = self
            .root
            .fork_session(true)
            .context("fork root for verification")?;
        session.port_plan = Some(ports.to_vec());
        let t = Trace {
            version: 2,
            argv: self.argv.clone(),
            pmu: self.pmu.name.to_string(),
            virtual_time: self.prefix.virtual_time,
            virtual_random: self.prefix.virtual_random,
            seed: None,
            execution: self.prefix.execution.clone(),
            env: self.prefix.env.clone(),
            cwd: self.prefix.cwd.clone(),
            ports: ports.to_vec(),
            steps: steps.to_vec(),
            outcome: expected.clone(),
        };
        let mut pol = Replay::new(&t);
        let mut out = Vec::new();
        session.replay = true;
        session.files_branch_start();
        let outcome = run_branch(&mut session, &mut pol, &self.ropts, usize::MAX, &mut out)?;
        let ok = match (&outcome, expected) {
            (Outcome::Bound, _) if pol.idx == steps.len() => true,
            (Outcome::Divergence { .. }, _) => false,
            (a, b) => a == b || pol.finished(),
        };
        if !ok {
            eprintln!("[ffs] verification diverged: expected {expected:?}, got {outcome:?} after {} of {} steps", pol.idx, steps.len());
        }
        session.kill_all();
        session.restore_files();
        Ok(ok)
    }

    fn report_branch(&mut self, steps: &[Step], outcome: &Outcome, ports: &[u16]) -> Result<bool> {
        self.branches += 1;
        if !outcome.is_failure() {
            return Ok(false);
        }
        let path = self.save_failure(steps, outcome, ports)?;
        eprintln!(
            "[ffs] branch {}: FAILURE {} after {} steps; schedule saved to {}",
            self.branches,
            summarize(outcome),
            steps.len(),
            path.display()
        );
        if !self.eopts.no_verify {
            let ok = self.verify(steps, outcome, ports)?;
            eprintln!(
                "[ffs]   re-execution from the checkpoint: {}",
                if ok { "reproduced" } else { "NOT reproduced" }
            );
        }
        Ok(true)
    }

    /// Install this branch's watchpoints: the explicit ones, or a random
    /// sample of the executable's writable globals.
    fn watch_branch(&mut self, session: &mut Session) -> Result<()> {
        let mut addrs = self.fixed_watch.clone();
        // Slots the program registered with ffs_watch() before the root.
        for &a in &self.root.watch_requests {
            if !addrs.contains(&a) && addrs.len() < 4 {
                addrs.push(a);
            }
        }
        if addrs.is_empty() && self.params.watch_count > 0 && !self.globals.is_empty() {
            // Distinct 8-byte slots covered by the globals (neighbouring
            // small variables share one).
            let mut slots: Vec<(u64, &str)> = Vec::new();
            for g in &self.globals {
                for i in 0..g.size.div_ceil(8).max(1) {
                    let a = (g.addr & !7) + 8 * i;
                    if !slots.iter().any(|(x, _)| *x == a) {
                        slots.push((a, g.name.as_str()));
                    }
                }
            }
            let n = (self.params.watch_count as usize).min(4).min(slots.len());
            let mut names = Vec::new();
            for i in 0..n {
                let j = self.rng.gen_range(i..slots.len());
                slots.swap(i, j);
                addrs.push(slots[i].0);
                names.push(slots[i].1.to_string());
            }
            if self.ropts.verbose {
                eprintln!("[ffs]   watching {}", names.join(", "));
            }
        }
        session.watchpoints = addrs;
        session.watch_hits = [0; 4];
        session.watch_disabled = 0;
        session.apply_watchpoints()
    }

    /// A fresh policy for one branch (random or PCT, with its own seed).
    fn branch_policy(&mut self) -> Result<Box<dyn Policy>> {
        let seed: u64 = self.rng.gen();
        let hint = self.last_len.max(1);
        sched::make_branch_policy(&self.params, seed, hint, self.branches)
    }

    fn explore_random(&mut self) -> Result<()> {
        let start = Instant::now();
        for i in 0..self.eopts.iterations {
            let mut session = self
                .root
                .fork_session(true)
                .with_context(|| format!("fork root for branch {i}"))?;
            let mut pol = self.branch_policy()?;
            self.watch_branch(&mut session)?;
            session.files_branch_start();
            let mut steps = Vec::new();
            let outcome = run_branch(
                &mut session,
                pol.as_mut(),
                &self.ropts,
                self.eopts.max_depth,
                &mut steps,
            )?;
            let ports = session.ports_used.clone();
            session.kill_all();
            session.restore_files();
            drop(session);
            self.last_len = steps.len();
            if self.ropts.verbose || (i + 1).is_multiple_of(10) {
                eprintln!(
                    "[ffs] branch {}: {} ({} steps, {:.1}s elapsed)",
                    i + 1,
                    summarize(&outcome),
                    steps.len(),
                    start.elapsed().as_secs_f64()
                );
            }
            if self.report_branch(&steps, &outcome, &ports)? && !self.eopts.keep_going {
                return Ok(());
            }
        }
        Ok(())
    }

    fn explore_dfs(&mut self) -> Result<()> {
        struct Node {
            /// Enabled threads at this node (the branching set).
            choices: Vec<Ltid>,
            /// Index of the next unexplored choice.
            next: usize,
            /// Frozen copy of the state at this node (if kept).
            ckpt: Option<Session>,
            /// Steps that led from the parent to this node (a `Fork` step
            /// when a checkpoint was taken at the parent, then the step).
            steps_in: Vec<Step>,
        }
        let start = Instant::now();
        let mut live = self.root.fork_session(true)?;
        self.watch_branch(&mut live)?;
        live.files_branch_start();
        let mut stack: Vec<Node> = Vec::new();
        // Root node.
        let enabled = match observe(&mut live, None)? {
            Ok(e) => e,
            Err(o) => {
                eprintln!("[ffs] program terminates at the root: {}", summarize(&o));
                return Ok(());
            }
        };
        stack.push(Node {
            choices: enabled,
            next: 0,
            ckpt: None,
            steps_in: vec![],
        });
        let mut iterations = 0usize;
        'branches: loop {
            // Descend from the top of the stack along unexplored choices.
            let outcome = loop {
                let depth = stack.len();
                let live_ckpts = stack.iter().filter(|n| n.ckpt.is_some()).count();
                let top = stack.last_mut().unwrap();
                if top.next >= top.choices.len() {
                    unreachable!("node without choices on the stack");
                }
                let forced = forced_choices(&mut live, &top.choices.clone());
                let (ltid, branching) = if !forced.is_empty() {
                    top.next = top.choices.len();
                    (forced[0], false)
                } else {
                    let l = top.choices[top.next];
                    top.next += 1;
                    (l, top.next < top.choices.len())
                };
                // Keep a checkpoint if we will have to come back here.  The
                // normalization performed by the fork is part of the path.
                let mut steps_in = Vec::new();
                if branching && top.ckpt.is_none() && live_ckpts < self.eopts.max_checkpoints {
                    top.ckpt = Some(live.fork_session(false).context("checkpoint at node")?);
                    steps_in.push(Step {
                        ltid: 0,
                        event: Event::Fork,
                        ticks: 0,
                        rip: 0,
                        effects: None,
                        vt: 0,
                    });
                }
                let mut budget =
                    if self.params.max_budget > 0 && self.rng.gen_bool(self.params.preempt_prob) {
                        log_uniform(&mut self.rng, self.params.max_budget)
                    } else {
                        0
                    };
                if self.params.max_slice > 0
                    && top.choices.len() > 1
                    && (budget == 0 || budget > self.params.max_slice)
                {
                    budget = self.params.max_slice;
                }
                let step = perform(&mut live, &Decision::Run { ltid, budget, steps: 0 })?;
                if self.ropts.verbose {
                    eprintln!(
                        "[ffs]   d{depth} T{} ticks={} rip={:#x} {}",
                        step.ltid,
                        step.ticks,
                        step.rip,
                        describe(&step.event)
                    );
                }
                let is_end = step.event == Event::End;
                let ev_outcome = event_outcome(&step.event, &self.ropts);
                steps_in.push(step);
                if is_end {
                    stack.push(Node {
                        choices: vec![],
                        next: 0,
                        ckpt: None,
                        steps_in,
                    });
                    let mut fifo = sched::Fifo::new(DRAIN_SLICE);
                    let mut rest = Vec::new();
                    let o = run_branch(
                        &mut live,
                        &mut fifo,
                        &self.ropts,
                        self.eopts.max_depth,
                        &mut rest,
                    )?;
                    stack.push(Node {
                        choices: vec![],
                        next: 0,
                        ckpt: None,
                        steps_in: rest,
                    });
                    break o;
                }
                if let Some(o) = ev_outcome {
                    stack.push(Node {
                        choices: vec![],
                        next: 0,
                        ckpt: None,
                        steps_in,
                    });
                    break o;
                }
                let enabled = match observe(&mut live, None)? {
                    Ok(e) => e,
                    Err(o) => {
                        stack.push(Node {
                            choices: vec![],
                            next: 0,
                            ckpt: None,
                            steps_in,
                        });
                        break o;
                    }
                };
                if depth >= self.eopts.max_depth {
                    stack.push(Node {
                        choices: vec![],
                        next: 0,
                        ckpt: None,
                        steps_in,
                    });
                    break Outcome::Bound;
                }
                stack.push(Node {
                    choices: enabled,
                    next: 0,
                    ckpt: None,
                    steps_in,
                });
            };
            iterations += 1;
            let steps: Vec<Step> = stack
                .iter()
                .flat_map(|n| n.steps_in.iter().cloned())
                .collect();
            if self.ropts.verbose || iterations.is_multiple_of(10) {
                eprintln!(
                    "[ffs] branch {iterations}: {} ({} steps, {} live checkpoints, {:.1}s)",
                    summarize(&outcome),
                    steps.len(),
                    stack.iter().filter(|n| n.ckpt.is_some()).count(),
                    start.elapsed().as_secs_f64()
                );
            }
            live.kill_all();
            if self.report_branch(&steps, &outcome, &live.ports_used.clone())? && !self.eopts.keep_going {
                return Ok(());
            }
            if iterations >= self.eopts.iterations {
                eprintln!("[ffs] iteration limit reached");
                return Ok(());
            }
            // Backtrack to the deepest node with an unexplored choice.
            while let Some(top) = stack.last() {
                if top.next < top.choices.len() {
                    break;
                }
                stack.pop();
            }
            if stack.is_empty() {
                eprintln!("[ffs] search space exhausted after {iterations} branches");
                return Ok(());
            }
            // Restore the live session at the top node: fork its checkpoint,
            // or fork the nearest ancestor's and replay the steps in between.
            let target = stack.len() - 1;
            let mut anc = target;
            while stack[anc].ckpt.is_none() {
                if anc == 0 {
                    // Re-execute from the root.
                    break;
                }
                anc -= 1;
            }
            // Files go back to the state the exploration started from (the
            // steps replayed below rewrite whatever the path wrote).
            live.restore_files();
            live = match stack[anc].ckpt.as_mut() {
                Some(c) => c
                    .fork_session(true)
                    .context("fork checkpoint for backtracking")?,
                None => self
                    .root
                    .fork_session(true)
                    .context("fork root for backtracking")?,
            };
            live.files_branch_start();
            let replay_steps: Vec<Step> = stack[anc + 1..=target]
                .iter()
                .flat_map(|n| n.steps_in.iter().cloned())
                .collect();
            if !replay_steps.is_empty() {
                let t = Trace {
                    version: 2,
                    argv: self.argv.clone(),
                    pmu: self.pmu.name.to_string(),
                    virtual_time: self.prefix.virtual_time,
                    virtual_random: self.prefix.virtual_random,
                    seed: None,
                    execution: self.prefix.execution.clone(),
                    env: self.prefix.env.clone(),
                    cwd: self.prefix.cwd.clone(),
                    ports: self.prefix.ports.clone(),
                    steps: replay_steps.clone(),
                    outcome: Outcome::Bound,
                };
                let mut rp = Replay::new(&t);
                let mut out = Vec::new();
                live.replay = true;
                let o = run_branch(&mut live, &mut rp, &self.ropts, usize::MAX, &mut out)?;
                live.replay = false;
                if !(o == Outcome::Bound && rp.idx == replay_steps.len()) {
                    return Err(anyhow!("re-execution to restore a node diverged ({o:?} after {} of {} steps); the program is not deterministic under ffs", rp.idx, replay_steps.len()));
                }
            }
            // Make sure the restored node's enabled set still matches.
            match observe(&mut live, None)? {
                Ok(e) => {
                    if e != stack[target].choices {
                        log::warn!(
                            "enabled set changed after restoring node {target}: {:?} vs {:?}",
                            e,
                            stack[target].choices
                        );
                    }
                }
                Err(o) => {
                    eprintln!("[ffs] restored node terminated unexpectedly: {o:?}");
                    stack.pop();
                    if stack.is_empty() {
                        return Ok(());
                    }
                    continue 'branches;
                }
            }
        }
    }
}

fn summarize(o: &Outcome) -> String {
    match o {
        Outcome::Exited { code } => format!("exited({code})"),
        Outcome::AssertFailed { msg } => format!("assertion failed: {msg}"),
        Outcome::Deadlock { blocked, .. } => format!("deadlock (blocked threads {blocked:?})"),
        Outcome::Hang { vtime_ns, runnable, .. } => format!(
            "hang ({:.1} virtual seconds without terminating; runnable threads {runnable:?})",
            *vtime_ns as f64 / 1e9
        ),
        Outcome::Killed { sig } => format!("killed by signal {sig}"),
        Outcome::Bound => "depth bound".to_string(),
        Outcome::End => "ffs_end".to_string(),
        Outcome::Divergence {
            at,
            expected,
            actual,
        } => format!("divergence at {at}: expected {expected}, got {actual}"),
        Outcome::Error { msg } => format!("error: {msg}"),
        Outcome::Paused => "paused".to_string(),
    }
}

/// A root given as a point in the run rather than an API call:
/// `vtime:SECS` (virtual seconds, deterministic), `steps:N` (scheduling
/// steps), `real:SECS` (wall-clock seconds since the start; where the
/// root lands then depends on machine speed, the recorded prefix still
/// replays).
#[derive(Clone, Copy, Debug)]
enum RootAfter {
    VirtualNs(u64),
    Steps(usize),
    Real(std::time::Duration),
}

impl RootAfter {
    fn parse(root: &str) -> Result<Option<RootAfter>> {
        let secs = |v: &str| -> Result<f64> {
            v.parse::<f64>().map_err(|_| anyhow::anyhow!("bad --root value {root:?}: expected seconds"))
        };
        Ok(match root.split_once(':') {
            Some(("vtime", v)) => Some(RootAfter::VirtualNs((secs(v)? * 1e9) as u64)),
            Some(("real", v)) => Some(RootAfter::Real(std::time::Duration::from_secs_f64(secs(v)?))),
            Some(("steps", v)) => Some(RootAfter::Steps(
                v.parse().map_err(|_| anyhow::anyhow!("bad --root value {root:?}: expected a step count"))?,
            )),
            Some(_) => anyhow::bail!("unknown --root {root:?}: start, begin, checkpoint, auto, vtime:SECS, steps:N or real:SECS"),
            None if matches!(root, "start" | "begin" | "checkpoint" | "auto") => None,
            None => anyhow::bail!("unknown --root {root:?}: start, begin, checkpoint, auto, vtime:SECS, steps:N or real:SECS"),
        })
    }
}

pub fn explore(
    argv: Vec<String>,
    pmu: PmuConfig,
    sopts: SessionOpts,
    ropts: RunOpts,
    params: PolicyParams,
    eopts: ExploreOpts,
) -> Result<i32> {
    let mut session = Session::spawn(&argv, pmu.clone(), sopts)?;
    let mut prefix = Trace::new(
        argv.clone(),
        pmu.name.to_string(),
        session.opts.virtual_time,
        Some(params.seed),
    );
    prefix.virtual_random = session.opts.virtual_random;
    prefix.capture_execution(&session);
    let root_kind = eopts.root.clone();

    // Phase 1: run to the root with a deterministic policy.
    if root_kind != "start" {
        let mut fifo = sched::Fifo::new(DRAIN_SLICE);
        let after = RootAfter::parse(&root_kind)?;
        let started = std::time::Instant::now();
        let mut steps = 0usize;
        let mut hook = |s: &mut Session, step: &Step| -> Result<Option<Outcome>> {
            steps += 1;
            let hit = match after {
                Some(RootAfter::VirtualNs(ns)) => s.virtual_ns() >= ns,
                Some(RootAfter::Steps(n)) => steps >= n,
                Some(RootAfter::Real(d)) => started.elapsed() >= d,
                None => matches!(
                    (&step.event, root_kind.as_str()),
                    (Event::Checkpoint { .. }, "auto" | "checkpoint")
                        | (Event::Begin, "auto" | "begin")
                ),
            };
            Ok(if hit { Some(Outcome::Paused) } else { None })
        };
        let outcome = sched::drive(
            &mut session,
            &mut fifo,
            &mut prefix,
            &ropts,
            Some(&mut hook),
        )?;
        if outcome != Outcome::Paused {
            if root_kind == "auto" {
                eprintln!("[ffs] no ffs_checkpoint()/ffs_begin() reached ({}); exploring from the start instead", summarize(&outcome));
                let opts = session.opts.clone();
                session.kill_all();
                session.restore_files();
                drop(session);
                session = Session::spawn(&argv, pmu.clone(), opts)?;
                prefix.steps.clear();
            } else {
                eprintln!(
                    "[ffs] program finished before reaching the root: {}",
                    summarize(&outcome)
                );
                return Ok(crate::report_code(&outcome));
            }
        } else if after.is_some() {
            eprintln!(
                "[ffs] exploration root after {} steps at virtual time {:.3} s ({} threads)",
                prefix.steps.len(),
                session.virtual_ns() as f64 / 1e9,
                session.alive_tasks().count()
            );
        } else {
            eprintln!(
                "[ffs] reached exploration root after {} steps: {}",
                prefix.steps.len(),
                describe(&prefix.steps.last().unwrap().event)
            );
        }
    }

    // Phase 2: checkpoint the root (a frozen copy); the original is killed so
    // that branches can reuse its canonical thread ids.
    prefix.ports = session.ports_used.clone();
    let root = session.fork_session(false).context("checkpoint the root")?;
    prefix.steps.push(Step {
        ltid: 0,
        event: Event::Fork,
        ticks: 0,
        rip: 0,
        effects: None,
        vt: 0,
    });
    session.kill_all();
    drop(session);
    eprintln!(
        "[ffs] root checkpoint taken ({} threads); strategy={}",
        root.alive_tasks().count(),
        eopts.strategy
    );

    let mut ex = Explorer {
        argv,
        pmu,
        ropts,
        rng: ChaCha8Rng::seed_from_u64(params.seed),
        params,
        eopts,
        prefix,
        root,
        failures: 0,
        branches: 0,
        last_len: 0,
        globals: Vec::new(),
        fixed_watch: Vec::new(),
    };
    if ex.params.watch_count > 0 || !ex.params.watch.is_empty() {
        match crate::watch::globals_of(ex.root.main_tgid) {
            Ok(g) => ex.globals = g,
            Err(e) => eprintln!("[ffs] warning: no globals to watch ({e:#})"),
        }
        ex.fixed_watch = ex
            .params
            .watch
            .iter()
            .map(|w| crate::watch::resolve(&ex.globals, w))
            .collect::<Result<_>>()?;
        if ex.params.watch.is_empty() && !ex.globals.is_empty() {
            eprintln!("[ffs] {} writable globals available for watchpoints; sampling {} per branch", ex.globals.len(), ex.params.watch_count.min(4));
        }
    }
    match ex.eopts.strategy.as_str() {
        "random" => ex.explore_random()?,
        "dfs" => ex.explore_dfs()?,
        other => return Err(anyhow!("unknown strategy {other}")),
    }
    eprintln!(
        "[ffs] explored {} branches, {} failing",
        ex.branches, ex.failures
    );
    // Put back the files the bootstrap phase wrote, so that a run leaves
    // the working directory as it found it.
    ex.root.restore_files();
    Ok(if ex.failures > 0 { 2 } else { 0 })
}
