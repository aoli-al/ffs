//! Schedule traces.  A trace is the complete sequence of scheduling decisions
//! and selected syscall outputs; replay checks the recorded observations.

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
    /// Observed syscall outputs. None means this syscall was not captured
    /// (or this is a legacy trace), not that it wrote no bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effects: Option<crate::effects::Effects>,
    /// Virtual time (ns) just before the step.  Whether the scheduler
    /// jumped the virtual clock while every thread was blocked depends on
    /// real I/O timing, so a replay sets its clock from this instead of
    /// deciding again (0 in traces that predate the field).
    #[serde(default)]
    pub vt: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Outcome {
    /// The main process exited with this code.
    Exited { code: i32 },
    /// `ffs_assert` failed in the tracee.
    AssertFailed { msg: String },
    /// No thread could make progress: `blocked` waits without a timeout,
    /// and for `wakes` virtual-deadline wakeups over `vtime_ns` nothing
    /// but the timer-driven threads `timers` ran (see
    /// `Session::deadlock_check`).
    Deadlock {
        blocked: Vec<Ltid>,
        #[serde(default)]
        timers: Vec<Ltid>,
        #[serde(default)]
        vtime_ns: u64,
        #[serde(default)]
        wakes: u64,
    },
    /// The program neither terminated nor deadlocked within `--max-vtime`
    /// virtual seconds: a livelock, a hang on a timer, or a long run.
    Hang {
        vtime_ns: u64,
        runnable: Vec<Ltid>,
        blocked: Vec<Ltid>,
    },
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
            Outcome::AssertFailed { .. } | Outcome::Deadlock { .. } | Outcome::Hang { .. } | Outcome::Killed { .. }
        ) || matches!(self, Outcome::Exited { code } if *code != 0)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionConfig {
    pub ticks: String,
    pub slice_cap_ns: u64,
    pub pid_namespace: bool,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionConfig>,
    /// Environment and working directory of the recorded run; a replay
    /// starts the program with the same ones.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub cwd: String,
    /// Ports ffs chose for the program's `bind`s to port 0, in order; a
    /// replay binds the same ones (see `Session::net_pre_syscall`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<u16>,
    pub steps: Vec<Step>,
    pub outcome: Outcome,
}

impl Trace {
    pub fn new(argv: Vec<String>, pmu: String, virtual_time: bool, seed: Option<u64>) -> Trace {
        Trace {
            version: 2,
            argv,
            pmu,
            virtual_time,
            virtual_random: false,
            seed,
            execution: None,
            env: std::env::vars().collect(),
            cwd: std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            ports: Vec::new(),
            steps: Vec::new(),
            outcome: Outcome::Bound,
        }
    }

    pub fn capture_execution(&mut self, session: &crate::session::Session) {
        use crate::perf::TicksSource;
        self.execution = Some(ExecutionConfig {
            ticks: match session.pmu.source {
                TicksSource::Hardware => "hw",
                TicksSource::Instructions => "insns",
                TicksSource::None => "none",
            }.into(),
            slice_cap_ns: session.opts.slice_cap.as_nanos() as u64,
            pid_namespace: session.opts.ns.is_some(),
        });
    }

    /// Session options that reproduce this trace's environment.
    pub fn apply_env(&self, opts: &mut crate::session::SessionOpts) {
        if let Some(config) = &self.execution {
            opts.slice_cap = std::time::Duration::from_nanos(config.slice_cap_ns);
            if !config.pid_namespace { opts.ns = None; }
        }
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
        let trace: Trace = serde_json::from_str(&s)?;
        anyhow::ensure!(matches!(trace.version, 1 | 2), "unsupported trace version {}", trace.version);
        Ok(trace)
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
        6 => "lstat",
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
        19 => "readv",
        20 => "writev",
        21 => "access",
        22 => "pipe",
        23 => "select",
        24 => "sched_yield",
        25 => "mremap",
        26 => "msync",
        27 => "mincore",
        28 => "madvise",
        29 => "shmget",
        30 => "shmat",
        31 => "shmctl",
        32 => "dup",
        33 => "dup2",
        34 => "pause",
        35 => "nanosleep",
        36 => "getitimer",
        37 => "alarm",
        38 => "setitimer",
        39 => "getpid",
        40 => "sendfile",
        41 => "socket",
        42 => "connect",
        43 => "accept",
        44 => "sendto",
        45 => "recvfrom",
        46 => "sendmsg",
        47 => "recvmsg",
        48 => "shutdown",
        49 => "bind",
        50 => "listen",
        51 => "getsockname",
        52 => "getpeername",
        53 => "socketpair",
        54 => "setsockopt",
        55 => "getsockopt",
        56 => "clone",
        57 => "fork",
        58 => "vfork",
        59 => "execve",
        60 => "exit",
        61 => "wait4",
        62 => "kill",
        63 => "uname",
        64 => "semget",
        65 => "semop",
        66 => "semctl",
        67 => "shmdt",
        72 => "fcntl",
        73 => "flock",
        74 => "fsync",
        75 => "fdatasync",
        76 => "truncate",
        77 => "ftruncate",
        78 => "getdents",
        79 => "getcwd",
        80 => "chdir",
        81 => "fchdir",
        82 => "rename",
        83 => "mkdir",
        84 => "rmdir",
        85 => "creat",
        86 => "link",
        87 => "unlink",
        88 => "symlink",
        89 => "readlink",
        90 => "chmod",
        91 => "fchmod",
        92 => "chown",
        93 => "fchown",
        95 => "umask",
        96 => "gettimeofday",
        97 => "getrlimit",
        98 => "getrusage",
        99 => "sysinfo",
        100 => "times",
        101 => "ptrace",
        102 => "getuid",
        104 => "getgid",
        105 => "setuid",
        106 => "setgid",
        107 => "geteuid",
        108 => "getegid",
        109 => "setpgid",
        110 => "getppid",
        111 => "getpgrp",
        112 => "setsid",
        115 => "getgroups",
        118 => "getresuid",
        120 => "getresgid",
        121 => "getpgid",
        124 => "getsid",
        125 => "capget",
        126 => "capset",
        127 => "rt_sigpending",
        128 => "rt_sigtimedwait",
        129 => "rt_sigqueueinfo",
        130 => "rt_sigsuspend",
        131 => "sigaltstack",
        132 => "utime",
        133 => "mknod",
        137 => "statfs",
        138 => "fstatfs",
        140 => "getpriority",
        141 => "setpriority",
        142 => "sched_setparam",
        143 => "sched_getparam",
        144 => "sched_setscheduler",
        145 => "sched_getscheduler",
        146 => "sched_get_priority_max",
        147 => "sched_get_priority_min",
        148 => "sched_rr_get_interval",
        149 => "mlock",
        150 => "munlock",
        157 => "prctl",
        158 => "arch_prctl",
        160 => "setrlimit",
        165 => "mount",
        166 => "umount2",
        186 => "gettid",
        187 => "readahead",
        191 => "getxattr",
        192 => "lgetxattr",
        193 => "fgetxattr",
        194 => "listxattr",
        196 => "flistxattr",
        200 => "tkill",
        201 => "time",
        202 => "futex",
        203 => "sched_setaffinity",
        204 => "sched_getaffinity",
        213 => "epoll_create",
        217 => "getdents64",
        218 => "set_tid_address",
        219 => "restart_syscall",
        222 => "timer_create",
        223 => "timer_settime",
        224 => "timer_gettime",
        225 => "timer_getoverrun",
        226 => "timer_delete",
        227 => "clock_settime",
        228 => "clock_gettime",
        229 => "clock_getres",
        230 => "clock_nanosleep",
        231 => "exit_group",
        232 => "epoll_wait",
        233 => "epoll_ctl",
        234 => "tgkill",
        235 => "utimes",
        237 => "mbind",
        238 => "set_mempolicy",
        239 => "get_mempolicy",
        247 => "waitid",
        251 => "ioprio_set",
        252 => "ioprio_get",
        253 => "inotify_init",
        254 => "inotify_add_watch",
        255 => "inotify_rm_watch",
        257 => "openat",
        258 => "mkdirat",
        259 => "mknodat",
        260 => "fchownat",
        261 => "futimesat",
        262 => "newfstatat",
        263 => "unlinkat",
        264 => "renameat",
        265 => "linkat",
        266 => "symlinkat",
        267 => "readlinkat",
        268 => "fchmodat",
        269 => "faccessat",
        270 => "pselect6",
        271 => "ppoll",
        272 => "unshare",
        273 => "set_robust_list",
        274 => "get_robust_list",
        275 => "splice",
        276 => "tee",
        277 => "sync_file_range",
        278 => "vmsplice",
        280 => "utimensat",
        281 => "epoll_pwait",
        282 => "signalfd",
        283 => "timerfd_create",
        284 => "eventfd",
        285 => "fallocate",
        286 => "timerfd_settime",
        287 => "timerfd_gettime",
        288 => "accept4",
        289 => "signalfd4",
        290 => "eventfd2",
        291 => "epoll_create1",
        292 => "dup3",
        293 => "pipe2",
        294 => "inotify_init1",
        295 => "preadv",
        296 => "pwritev",
        297 => "rt_tgsigqueueinfo",
        298 => "perf_event_open",
        299 => "recvmmsg",
        300 => "fanotify_init",
        301 => "fanotify_mark",
        302 => "prlimit64",
        303 => "name_to_handle_at",
        304 => "open_by_handle_at",
        305 => "clock_adjtime",
        306 => "syncfs",
        307 => "sendmmsg",
        308 => "setns",
        309 => "getcpu",
        310 => "process_vm_readv",
        311 => "process_vm_writev",
        312 => "kcmp",
        313 => "finit_module",
        314 => "sched_setattr",
        315 => "sched_getattr",
        316 => "renameat2",
        317 => "seccomp",
        318 => "getrandom",
        319 => "memfd_create",
        322 => "execveat",
        323 => "userfaultfd",
        324 => "membarrier",
        325 => "mlock2",
        326 => "copy_file_range",
        327 => "preadv2",
        328 => "pwritev2",
        329 => "pkey_mprotect",
        330 => "pkey_alloc",
        331 => "pkey_free",
        332 => "statx",
        333 => "io_pgetevents",
        334 => "rseq",
        424 => "pidfd_send_signal",
        425 => "io_uring_setup",
        426 => "io_uring_enter",
        427 => "io_uring_register",
        428 => "open_tree",
        429 => "move_mount",
        430 => "fsopen",
        431 => "fsconfig",
        432 => "fsmount",
        433 => "fspick",
        434 => "pidfd_open",
        435 => "clone3",
        436 => "close_range",
        437 => "openat2",
        438 => "pidfd_getfd",
        439 => "faccessat2",
        440 => "process_madvise",
        441 => "epoll_pwait2",
        442 => "mount_setattr",
        447 => "memfd_secret",
        448 => "process_mrelease",
        449 => "futex_waitv",
        452 => "fchmodat2",
        454 => "futex_wake",
        455 => "futex_wait",
        456 => "futex_requeue",
        _ => "?",
    }
}
