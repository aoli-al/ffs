//! ffs - fork-based deterministic concurrency testing.
//!
//! An rr-style ptrace tracer that serializes a multithreaded program onto a
//! deterministic scheduler, records/replays schedules using hardware tick
//! counters, and explores alternative schedules from checkpoints taken by
//! forking the (stopped) tracee.

mod checkpoint;
mod explore;
mod fdpass;
mod ffs_api;
mod files;
mod mem;
mod ns;
mod perf;
mod proc;
mod ptrace;
mod random;
mod remote;
mod sched;
mod session;
mod task;
mod trace;
mod vdso;
mod vtime;
mod watch;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;

use perf::{PmuConfig, TicksSource};
use session::{Session, SessionOpts};
use trace::{Outcome, Trace};

#[derive(Parser)]
#[command(
    name = "ffs",
    version,
    about = "fork-based deterministic concurrency testing"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args, Clone, Debug)]
struct CommonOpts {
    /// Ticks source: hw (retired conditional branches), insns, none.
    #[arg(long, default_value = "hw")]
    ticks: String,
    /// Let the tracee see real time (clocks, rdtsc, sleeps, timeouts and
    /// timers) instead of the virtual clock derived from its own progress.
    #[arg(long)]
    real_time: bool,
    /// Accepted for compatibility: virtual time is the default.
    #[arg(long, hide = true)]
    virtual_time: bool,
    /// Seconds to wait for a blocked thread before reporting a deadlock.
    #[arg(long, default_value_t = 3.0)]
    deadlock_timeout: f64,
    /// Microseconds a thread may sleep inside a syscall before it counts as
    /// blocked (the scheduler then switches away).
    #[arg(long, default_value_t = 100)]
    block_grace_us: u64,
    /// Milliseconds of real time a thread may run user code without any
    /// scheduling event before it is preempted regardless of ticks (a loop
    /// without conditional branches never reaches a tick budget).
    #[arg(long, default_value_t = 100)]
    slice_cap_ms: u64,
    /// Print every scheduling step.
    #[arg(short, long)]
    verbose: bool,
    /// Maximum number of scheduling steps per execution.
    #[arg(long, default_value_t = 10_000_000)]
    max_steps: usize,
    /// Do not run the tracee in a private PID namespace (thread ids will then
    /// differ between checkpoints and the original).
    #[arg(long)]
    no_pidns: bool,
    /// Let the tracee see real randomness (getrandom, /dev/urandom,
    /// AT_RANDOM) instead of a deterministic stream derived from the seed.
    #[arg(long)]
    real_random: bool,
    /// Do not restore the files the program writes after each branch of an
    /// exploration (by default every branch starts from the same files).
    #[arg(long)]
    no_isolate_files: bool,
}

#[derive(Args, Clone, Debug)]
struct PolicyOpts {
    /// Scheduling policy: mix (alternate random and pct per branch), random,
    /// pct, rr, fifo.
    #[arg(long, default_value = "mix")]
    policy: String,
    /// PCT: bug depth to target (number of priority change points + 1).
    #[arg(long, default_value_t = 3)]
    pct_depth: u32,
    /// After a tick budget expires, single-step up to this many more
    /// instructions (uniformly chosen), so that preemptions can land between
    /// any two instructions, not only at conditional branches.
    #[arg(long, default_value_t = 32)]
    max_insn_steps: u32,
    /// Globals to watch with hardware watchpoints (every access becomes a
    /// scheduling point): symbol names, `name+offset`, or hex addresses;
    /// at most 4.  Default: `explore` samples `--watch-count` writable
    /// globals of the executable per branch.
    #[arg(long, value_delimiter = ',')]
    watch: Vec<String>,
    /// Number of globals to sample per branch when `--watch` is not given
    /// (0 disables watchpoints).
    #[arg(long, default_value_t = 4)]
    watch_count: u32,
    /// Random seed.
    #[arg(long, default_value_t = 0)]
    seed: u64,
    /// Probability of running a thread with a random tick budget (preemption).
    #[arg(long, default_value_t = 0.8)]
    preempt_prob: f64,
    /// Maximum random tick budget, log-uniformly distributed (0 disables
    /// tick-based preemption).
    #[arg(long, default_value_t = 5_000)]
    max_budget: u64,
    /// Probability of switching threads at an event.
    #[arg(long, default_value_t = 0.5)]
    switch_prob: f64,
    /// Maximum ticks a thread may run while others are runnable (0 = unlimited).
    #[arg(long, default_value_t = 1_000_000)]
    max_slice: u64,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check that the hardware tick counter works on this machine.
    Check,
    /// Run a program once under the deterministic scheduler and record the schedule.
    Record {
        #[command(flatten)]
        common: CommonOpts,
        #[command(flatten)]
        policy: PolicyOpts,
        /// Where to write the trace (JSON).
        #[arg(short, long, default_value = "ffs-trace.json")]
        output: PathBuf,
        /// Stop (and report success) when the tracee calls ffs_end().
        #[arg(long)]
        stop_at_end: bool,
        /// Command to run.
        #[arg(required = true, last = true)]
        argv: Vec<String>,
    },
    /// Replay a recorded schedule and check that the execution is identical.
    Replay {
        #[command(flatten)]
        common: CommonOpts,
        /// Trace to replay.
        trace: PathBuf,
        /// Compare syscall return values too.
        #[arg(long)]
        strict: bool,
        /// Command to run (defaults to the one in the trace).
        #[arg(last = true)]
        argv: Vec<String>,
    },
    /// Explore different schedules from a checkpoint.
    Explore {
        #[command(flatten)]
        common: CommonOpts,
        #[command(flatten)]
        policy: PolicyOpts,
        #[command(flatten)]
        explore: explore::ExploreOpts,
        /// Command to run.
        #[arg(required = true, last = true)]
        argv: Vec<String>,
    },
    /// Print a trace in human-readable form.
    Show { trace: PathBuf },
}

fn pmu_for(name: &str) -> Result<PmuConfig> {
    Ok(match name {
        "hw" => perf::detect_pmu()?,
        "insns" => perf::instructions_pmu(),
        "none" => perf::no_pmu(),
        other => bail!("unknown ticks source {other}"),
    })
}

fn session_opts(c: &CommonOpts, seed: u64) -> SessionOpts {
    let ns = if c.no_pidns {
        None
    } else {
        match ns::PidNs::setup() {
            Ok(ns) => Some(std::rc::Rc::new(ns)),
            Err(e) => {
                eprintln!("[ffs] warning: no private pid namespace ({e:#}); thread ids will not be stable across checkpoints");
                None
            }
        }
    };
    SessionOpts {
        virtual_time: !c.real_time,
        virtual_random: !c.real_random,
        isolate_files: false,
        block_grace: Duration::from_micros(c.block_grace_us),
        seed,
        deadlock_timeout: Duration::from_secs_f64(c.deadlock_timeout),
        env: None,
        cwd: None,
        slice_cap: Duration::from_millis(c.slice_cap_ms),
        ns,
    }
}

fn run_opts(c: &CommonOpts, stop_at_end: bool) -> sched::RunOpts {
    sched::RunOpts {
        max_steps: c.max_steps,
        stop_at_end,
        verbose: c.verbose,
    }
}

pub fn report_code(outcome: &Outcome) -> i32 {
    report(outcome)
}

fn report(outcome: &Outcome) -> i32 {
    match outcome {
        Outcome::Exited { code: 0 } => {
            eprintln!("[ffs] program exited normally");
            0
        }
        Outcome::Exited { code } => {
            eprintln!("[ffs] program exited with code {code}");
            *code
        }
        Outcome::AssertFailed { msg } => {
            eprintln!("[ffs] ASSERTION FAILED: {msg}");
            2
        }
        Outcome::Deadlock { blocked } => {
            eprintln!("[ffs] DEADLOCK: no thread can make progress (blocked: {blocked:?})");
            3
        }
        Outcome::Killed { sig } => {
            eprintln!("[ffs] program killed by signal {sig}");
            128 + sig
        }
        Outcome::Bound => {
            eprintln!("[ffs] step bound reached");
            4
        }
        Outcome::End => {
            eprintln!("[ffs] reached ffs_end()");
            0
        }
        Outcome::Divergence {
            at,
            expected,
            actual,
        } => {
            eprintln!(
                "[ffs] REPLAY DIVERGED at step {at}:\n  expected: {expected}\n  actual:   {actual}"
            );
            5
        }
        Outcome::Error { msg } => {
            eprintln!("[ffs] error: {msg}");
            6
        }
        Outcome::Paused => 0,
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    session::install_tracer_signal_handlers();
    let cli = Cli::parse();
    let code = match real_main(cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[ffs] error: {e:#}");
            70
        }
    };
    std::process::exit(code);
}

fn real_main(cli: Cli) -> Result<i32> {
    match cli.cmd {
        Cmd::Check => {
            let pmu = perf::detect_pmu().context("detect PMU")?;
            println!(
                "PMU: {} (config {:#x}, skid {})",
                pmu.name, pmu.config, pmu.skid
            );
            let ticks = perf::self_test(&pmu)?;
            println!("self-test: counted {ticks} ticks for 500 conditional branches: OK");
            let paranoid =
                std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid").unwrap_or_default();
            println!("perf_event_paranoid = {}", paranoid.trim());
            Ok(0)
        }
        Cmd::Show { trace } => {
            let t = Trace::load(&trace)?;
            print!("{}", t.render());
            Ok(0)
        }
        Cmd::Record {
            common,
            policy,
            output,
            stop_at_end,
            argv,
        } => {
            let pmu = pmu_for(&common.ticks)?;
            if pmu.source == TicksSource::None && policy.max_budget > 0 {
                log::warn!("no ticks source: tick-based preemption disabled");
            }
            let opts = session_opts(&common, policy.seed);
            let mut session = Session::spawn(&argv, pmu.clone(), opts.clone())?;
            let params: sched::PolicyParams = policy.clone().into();
            let mut pol = sched::make_policy(&params, policy.seed, 1000)?;
            if !params.watch.is_empty() {
                let globals = watch::globals_of(session.main_tgid)?;
                session.watchpoints = params.watch.iter().map(|w| watch::resolve(&globals, w)).collect::<Result<_>>()?;
                session.apply_watchpoints()?;
            }
            let mut trace = Trace::new(
                argv.clone(),
                pmu.name.to_string(),
                opts.virtual_time,
                Some(policy.seed),
            );
            trace.virtual_random = opts.virtual_random;
            let outcome = sched::drive(
                &mut session,
                pol.as_mut(),
                &mut trace,
                &run_opts(&common, stop_at_end),
                None,
            )?;
            trace.save(&output)?;
            eprintln!(
                "[ffs] recorded {} steps to {}",
                trace.steps.len(),
                output.display()
            );
            Ok(report(&outcome))
        }
        Cmd::Replay {
            common,
            trace,
            strict,
            argv,
        } => {
            let recorded = Trace::load(&trace)?;
            let argv = if argv.is_empty() {
                recorded.argv.clone()
            } else {
                argv
            };
            let pmu = pmu_for(&common.ticks)?;
            let mut opts = session_opts(&common, recorded.seed.unwrap_or(0));
            opts.virtual_time = recorded.virtual_time;
            opts.virtual_random = recorded.virtual_random;
            recorded.apply_env(&mut opts);
            let mut session = Session::spawn(&argv, pmu.clone(), opts)?;
            session.replay = true;
            let mut pol = sched::Replay::new(&recorded);
            pol.strict_ret = strict;
            let mut out = Trace::new(
                argv,
                pmu.name.to_string(),
                recorded.virtual_time,
                recorded.seed,
            );
            out.virtual_random = recorded.virtual_random;
            let outcome = sched::drive(
                &mut session,
                &mut pol,
                &mut out,
                &run_opts(&common, false),
                None,
            )?;
            if !matches!(outcome, Outcome::Divergence { .. })
                && outcome != recorded.outcome
                && outcome != Outcome::Bound
            {
                eprintln!(
                    "[ffs] note: recorded outcome was {:?}, replay outcome is {:?}",
                    recorded.outcome, outcome
                );
            }
            let complete = !matches!(outcome, Outcome::Divergence { .. })
                && (pol.idx == recorded.steps.len() || pol.finished());
            if complete {
                eprintln!(
                    "[ffs] replayed all {} steps identically",
                    recorded.steps.len()
                );
            } else {
                eprintln!(
                    "[ffs] replayed {} of {} steps",
                    pol.idx,
                    recorded.steps.len()
                );
            }
            if outcome == Outcome::Bound {
                return Ok(0);
            }
            Ok(report(&outcome))
        }
        Cmd::Explore {
            common,
            policy,
            explore: eopts,
            argv,
        } => {
            let pmu = pmu_for(&common.ticks)?;
            let mut sopts = session_opts(&common, policy.seed);
            sopts.isolate_files = !common.no_isolate_files;
            explore::explore(
                argv,
                pmu,
                sopts,
                run_opts(&common, false),
                policy.into(),
                eopts,
            )
        }
    }
}

impl From<PolicyOpts> for sched::PolicyParams {
    fn from(p: PolicyOpts) -> Self {
        sched::PolicyParams {
            name: p.policy,
            seed: p.seed,
            preempt_prob: p.preempt_prob,
            max_budget: p.max_budget,
            switch_prob: p.switch_prob,
            max_slice: p.max_slice,
            max_steps: p.max_insn_steps,
            pct_depth: p.pct_depth,
            watch: p.watch,
            watch_count: p.watch_count,
        }
    }
}
