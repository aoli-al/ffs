//! Schedule traces.  A trace is the complete sequence of scheduling decisions
//! and the events they led to; replaying it reproduces the execution.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::session::Event;
use crate::task::Ltid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Step {
    /// Logical thread that was run.
    pub ltid: Ltid,
    /// Event that ended the step.
    pub event: Event,
    /// Cumulative ticks of the thread at the event.
    pub ticks: u64,
    /// Instruction pointer at the event.
    pub rip: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Outcome {
    /// The main process exited with this code.
    Exited { code: i32 },
    /// `ffs_assert` failed in the tracee.
    AssertFailed { msg: String },
    /// No thread could make progress.
    Deadlock { blocked: Vec<Ltid> },
    /// The tracee was killed by a signal (e.g. SIGSEGV, SIGABRT).
    Killed { sig: i32 },
    /// The step bound was reached.
    Bound,
    /// Reached `ffs_end()` (exploration stops here).
    End,
    /// Replay did not follow the trace.
    Divergence {
        at: usize,
        expected: String,
        actual: String,
    },
    /// Internal error.
    Error { msg: String },
    /// Execution paused at the exploration root.
    Paused,
}

impl Outcome {
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            Outcome::AssertFailed { .. } | Outcome::Deadlock { .. } | Outcome::Killed { .. }
        ) || matches!(self, Outcome::Exited { code } if *code != 0)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trace {
    pub version: u32,
    pub argv: Vec<String>,
    pub pmu: String,
    pub virtual_time: bool,
    /// Randomness was virtualized (derived from `seed`).
    #[serde(default)]
    pub virtual_random: bool,
    pub seed: Option<u64>,
    /// Environment and working directory of the recorded run; a replay
    /// starts the program with the same ones.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub cwd: String,
    pub steps: Vec<Step>,
    pub outcome: Outcome,
}

impl Trace {
    pub fn new(argv: Vec<String>, pmu: String, virtual_time: bool, seed: Option<u64>) -> Trace {
        Trace {
            version: 1,
            argv,
            pmu,
            virtual_time,
            virtual_random: false,
            seed,
            env: std::env::vars().collect(),
            cwd: std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            steps: Vec::new(),
            outcome: Outcome::Bound,
        }
    }

    /// Session options that reproduce this trace's environment.
    pub fn apply_env(&self, opts: &mut crate::session::SessionOpts) {
        if !self.env.is_empty() {
            opts.env = Some(self.env.clone());
        }
        if !self.cwd.is_empty() {
            opts.cwd = Some(self.cwd.clone());
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let s = serde_json::to_string_pretty(self)?;
        std::fs::write(path, s).with_context(|| format!("write trace {}", path.display()))?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Trace> {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("read trace {}", path.display()))?;
        Ok(serde_json::from_str(&s)?)
    }

    /// Human-readable rendering of the schedule.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (i, s) in self.steps.iter().enumerate() {
            out.push_str(&format!(
                "{i:5}  T{:<3} ticks={:<10} rip={:#x}  {}\n",
                s.ltid,
                s.ticks,
                s.rip,
                describe(&s.event)
            ));
        }
        out.push_str(&format!("outcome: {:?}\n", self.outcome));
        out
    }
}

pub fn describe(ev: &Event) -> String {
    match ev {
        Event::SyscallEntry { nr } => format!("syscall {} ({nr})", syscall_name(*nr)),
        Event::SyscallExit { nr, ret, child } => match child {
            Some(c) => format!("  -> {} = {ret} (new thread T{c})", syscall_name(*nr)),
            None => format!("  -> {} = {ret}", syscall_name(*nr)),
        },
        Event::Blocked { nr } => format!("  blocked in {}", syscall_name(*nr)),
        Event::Preempt => "preempted".to_string(),
        Event::Slice => "preempted (slice cap)".to_string(),
        Event::Signal { sig, .. } => format!("signal {sig}"),
        Event::Exit => "exiting".to_string(),
        Event::Exited { code } => format!("exited ({code})"),
        Event::Checkpoint { name } => format!("ffs_checkpoint(\"{name}\")"),
        Event::Begin => "ffs_begin()".to_string(),
        Event::End => "ffs_end()".to_string(),
        Event::Yield => "ffs_yield()".to_string(),
        Event::AssertFailed { msg } => format!("ffs_assert failed: {msg}"),
        Event::Fork => "checkpoint".to_string(),
    }
}

pub fn syscall_name(nr: u64) -> &'static str {
    match nr {
        0 => "read",
        1 => "write",
        2 => "open",
        3 => "close",
        4 => "stat",
        5 => "fstat",
        7 => "poll",
        8 => "lseek",
        9 => "mmap",
        10 => "mprotect",
        11 => "munmap",
        12 => "brk",
        13 => "rt_sigaction",
        14 => "rt_sigprocmask",
        15 => "rt_sigreturn",
        16 => "ioctl",
        17 => "pread64",
        18 => "pwrite64",
        20 => "writev",
        21 => "access",
        22 => "pipe",
        23 => "select",
        24 => "sched_yield",
        25 => "mremap",
        28 => "madvise",
        32 => "dup",
        33 => "dup2",
        34 => "pause",
        35 => "nanosleep",
        37 => "alarm",
        38 => "setitimer",
        39 => "getpid",
        56 => "clone",
        57 => "fork",
        58 => "vfork",
        59 => "execve",
        60 => "exit",
        61 => "wait4",
        62 => "kill",
        63 => "uname",
        72 => "fcntl",
        96 => "gettimeofday",
        97 => "getrlimit",
        102 => "getuid",
        131 => "sigaltstack",
        158 => "arch_prctl",
        186 => "gettid",
        200 => "tkill",
        201 => "time",
        202 => "futex",
        203 => "sched_setaffinity",
        204 => "sched_getaffinity",
        218 => "set_tid_address",
        228 => "clock_gettime",
        229 => "clock_getres",
        230 => "clock_nanosleep",
        231 => "exit_group",
        232 => "epoll_wait",
        234 => "tgkill",
        257 => "openat",
        262 => "newfstatat",
        270 => "pselect6",
        271 => "ppoll",
        273 => "set_robust_list",
        302 => "prlimit64",
        318 => "getrandom",
        334 => "rseq",
        435 => "clone3",
        _ => "?",
    }
}
