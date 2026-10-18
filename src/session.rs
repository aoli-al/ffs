//! A `Session` owns a set of traced tasks and implements the serializing
//! execution model: exactly one tracee thread runs at a time, and the
//! scheduler only makes decisions at well-defined stops.

use anyhow::{anyhow, bail, Context, Result};
use crate::mem::Mem;
use libc::pid_t;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::ffs_api::{self, MagicCall};
use crate::ns::{self, PidNs};
use crate::perf::PmuConfig;
use crate::random::VirtualRandom;
use crate::ptrace::{self, ResumeHow, WaitStatus, RDTSCP_INSN, RDTSC_INSN, TIME_SLICE_SIGNAL};
use crate::task::{Ltid, State, Stop, Task};
use crate::vdso;
use crate::proc::VZombie;
use crate::vtime::{Emulation, VTimer};

/// Scheduler-visible events, produced by `Session::run_task`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Event {
    /// Stopped before executing syscall `nr`.
    SyscallEntry {
        nr: u64,
    },
    /// Syscall `nr` completed.  `child` is set for clone/fork/vfork.
    SyscallExit {
        nr: u64,
        ret: i64,
        child: Option<Ltid>,
    },
    /// The task entered syscall `nr` and blocked in the kernel.
    Blocked {
        nr: u64,
    },
    /// The tick budget expired.
    Preempt,
    /// Preempted by the real-time slice cap: the thread ran user code
    /// without reaching a tick budget (a loop without conditional
    /// branches).  Replayed like `Preempt`; advances the virtual clock by
    /// the cap, since the thread's ticks do not.
    Slice,
    /// A signal is about to be delivered (on the next resume).  `info` is
    /// the raw `siginfo_t` (hex), so that a replay can inject the signal at
    /// exactly this point with the same payload.
    Signal {
        sig: i32,
        #[serde(default)]
        info: String,
    },
    /// The thread is about to exit (PTRACE_EVENT_EXIT).
    Exit,
    /// The thread has been reaped.
    Exited {
        code: i32,
    },
    /// Magic calls from the tracee (see include/ffs.h).
    Checkpoint {
        name: String,
    },
    Begin,
    End,
    Yield,
    AssertFailed {
        msg: String,
    },
    /// A checkpoint was taken here (all threads were normalized).
    Fork,
}

#[derive(Clone)]
pub struct SessionOpts {
    pub virtual_time: bool,
    /// Replace kernel randomness by a stream derived from `seed`.
    pub virtual_random: bool,
    /// Snapshot files the program writes, to restore them between branches.
    pub isolate_files: bool,
    /// How long a thread must sit in the kernel ('S'/'D') before its
    /// syscall counts as blocking.  Under the serial scheduler nothing can
    /// wake a thread waiting on another tracee, so this only needs to cover
    /// transient sleeps inside a syscall that completes by itself.
    pub block_grace: Duration,
    pub seed: u64,
    /// Seconds to wait for a blocked thread before declaring a deadlock.
    pub deadlock_timeout: Duration,
    /// Deadlock detector (virtual time): report a deadlock once this many
    /// consecutive wakeups were caused by virtual deadlines alone, over at
    /// least `deadlock_vtime`, while some thread waits with no timeout.
    /// 0 disables it.
    pub deadlock_wakes: u64,
    pub deadlock_vtime: Duration,
    /// Virtual time after which a run that has not ended is a `Hang`
    /// (0 = unlimited).
    pub max_vtime: Duration,
    /// Environment and working directory for the tracee (None: inherit).
    /// A replay sets them from the trace: the environment is an input the
    /// dynamic loader and libc parse, so it changes tick counts.
    pub env: Option<Vec<(String, String)>>,
    pub cwd: Option<String>,
    /// Real time a thread may run user code without a tick-counted
    /// preemption before it is interrupted anyway (a loop without
    /// conditional branches never overflows the counter).  The interruption
    /// is recorded as an ordinary `Preempt` at (ticks, rip).
    pub slice_cap: Duration,
    /// Private pid namespace (stable thread ids), if available.
    pub ns: Option<Rc<PidNs>>,
}

impl Default for SessionOpts {
    fn default() -> Self {
        SessionOpts {
            deadlock_wakes: 200,
            deadlock_vtime: Duration::from_secs(30),
            max_vtime: Duration::from_secs(300),
            virtual_time: false,
            virtual_random: true,
            isolate_files: false,
            block_grace: Duration::from_micros(100),
            seed: 0,
            deadlock_timeout: Duration::from_secs(3),
            env: None,
            cwd: None,
            slice_cap: Duration::from_millis(100),
            ns: None,
        }
    }
}

/// Virtual clock used when `virtual_time` is on: time advances with ticks.
#[derive(Clone, Debug)]
pub struct VirtualClock {
    pub base_realtime_ns: u64,
    pub base_monotonic_ns: u64,
    /// Extra nanoseconds added per scheduler event (so time moves even for
    /// threads that only make syscalls).
    pub events: u64,
    /// Time skipped while every thread was waiting (see `vtime.rs`).
    pub skew: u64,
}

pub struct Session {
    pub cpu_online: Option<Rc<CpuOnline>>,
    pub tasks: BTreeMap<pid_t, Task>,
    pub ltids: BTreeMap<Ltid, pid_t>,
    pub next_ltid: Ltid,
    pub pmu: PmuConfig,
    pub opts: SessionOpts,
    pub vclock: VirtualClock,
    /// Tid of the initial process (its exit code is the session exit code).
    pub main_tgid: pid_t,
    /// Tids that exited but could not be reaped yet (zombie group leaders).
    pub zombies: Vec<pid_t>,
    /// Log lines emitted by the tracee through `ffs_log`.
    pub logs: Vec<String>,
    /// Deterministic randomness (see `random.rs`).
    pub vrandom: VirtualRandom,
    /// The session follows a recorded schedule: asynchronous signals from
    /// outside the session that the trace does not expect are discarded
    /// (a real timer still armed during a re-execution, a stray SIGINT).
    pub replay: bool,
    /// Signal that `deliver_signal` is making arrive right now.
    pub expected_signal: Option<i32>,
    /// Virtual timers (see `vtime.rs`).
    pub vtimers: Vec<VTimer>,
    /// siginfo to install when thread `ltid` next stops for signal `sig`
    /// (signals the tracer sent on behalf of an emulated kernel facility).
    pub siginfo_queue: Vec<(Ltid, i32, [u8; 128])>,
    /// Emulated process tree (see `proc.rs`): child process -> parent.
    pub parent_of: BTreeMap<Ltid, Ltid>,
    /// Exited children not yet waited for.
    pub vzombies: Vec<VZombie>,
    /// Processes in an emulated group stop, and the stop/continue events
    /// their parents have not waited for (`proc.rs`).
    pub stopped_procs: std::collections::BTreeSet<Ltid>,
    pub jobstops: Vec<crate::proc::VJobStop>,
    pub jobconts: Vec<crate::proc::VJobStop>,
    /// `PR_SET_PDEATHSIG` of each process, delivered when its (emulated)
    /// parent exits.
    pub pdeathsig: BTreeMap<Ltid, i32>,
    /// Processes (host tgid) killed by ffs with SIGKILL on behalf of a
    /// fatal signal the kernel would not let a traced namespace init take:
    /// the wait status their parent sees.
    pub exit_override: BTreeMap<pid_t, i32>,
    /// Processes whose exit has been processed.
    pub exited_procs: std::collections::BTreeSet<Ltid>,
    /// Peer socket inodes of socketpairs created by tracees, both ways.
    pub socket_peers: BTreeMap<u64, u64>,
    /// Addresses watched with hardware watchpoints (see `watch.rs`).
    pub watchpoints: Vec<u64>,
    /// 8-byte slots the program asked to have watched (`ffs_watch`): heap
    /// words that have no symbol.  `explore` adds them to every branch.
    pub watch_requests: Vec<u64>,
    /// Threads that still need their debug registers programmed.
    pub watch_pending: std::collections::BTreeSet<pid_t>,
    /// Hits per watchpoint slot in this execution, and slots disabled
    /// because they were hit too often.
    pub watch_hits: [u32; 4],
    pub watch_disabled: u8,
    /// Files to put back when this execution is over (see `files.rs`).
    pub file_snapshots: BTreeMap<std::path::PathBuf, crate::files::Snapshot>,
    /// The last `Preempt` event came from a watchpoint (the scheduling
    /// point is the access itself; no instruction steps are added).
    pub watch_preempt: bool,
    /// Progress bookkeeping for the deadlock detector.
    pub progress: Progress,
    /// Virtual time at which each thread was created.
    pub created_at: BTreeMap<Ltid, u64>,
    /// Virtual time of the first write to stdout or stderr.
    pub first_output: Option<u64>,
    /// Replay: threads given a pending preemption signal before a syscall
    /// (`Decision::RunPending`); the signal stop that follows the syscall
    /// exit is the recorded `Preempt`.
    pub prearmed: std::collections::BTreeSet<pid_t>,
    /// Ports chosen for `bind`s to port 0 so far, the recorded plan a
    /// replay follows, and the next port to hand out.
    pub ports_used: Vec<u16>,
    pub port_plan: Option<Vec<u16>>,
    pub next_port: u16,
    /// The last stop was the slice cap's PTRACE_INTERRUPT (see `wait_task`).
    pub cap_preempt: bool,
}

const CLOCK_GETTIME: u64 = 228;
/// Deadline wakeups after which a thread counts as periodic housekeeping
/// for the deadlock detector.
const PERIODIC_WAKES: u32 = 3;
const BIND: u64 = 49;
const AF_INET: u16 = 2;
const AF_INET6: u16 = 10;
/// Ports handed out for `bind(port 0)`, cycling through a range the kernel
/// does not use for its own ephemeral ports (32768-60999 by default).
const FIRST_PORT: u16 = 20000;
const LAST_PORT: u16 = 29999;
/// The x86 resume flag.
const X86_RF: u64 = 1 << 16;
const KILL: u64 = 62;
const TKILL: u64 = 200;
const TGKILL: u64 = 234;
const RT_SIGQUEUEINFO: u64 = 129;
const RT_TGSIGQUEUEINFO: u64 = 297;
const PIDFD_SEND_SIGNAL: u64 = 424;
const WRITE: u64 = 1;
const WRITEV: u64 = 20;
const RESTART_SYSCALL: u64 = 219;
/// `si_code` of a SIGTRAP caused by the trap flag.
const TRAP_TRACE: i32 = 2;
const FORK: u64 = 57;
const VFORK: u64 = 58;
const GETTIMEOFDAY: u64 = 96;
const TIME: u64 = 201;
const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;
const EXECVE: u64 = 59;
const GETCPU: u64 = 309;
const ADJTIMEX: u64 = 159;
const CLOCK_ADJTIME: u64 = 305;
const TIMES: u64 = 100;
const GETRUSAGE: u64 = 98;
const SCHED_GETAFFINITY: u64 = 204;
const PRCTL: u64 = 157;
const NANOSLEEP: u64 = 35;
const WAIT4: u64 = 61;
const WAITID: u64 = 247;
const CLOCK_NANOSLEEP: u64 = 230;
const RSEQ: u64 = 334;
const SOCKETPAIR: u64 = 53;
const RSEQ_FLAG_UNREGISTER: u64 = 1;

pub enum WaitOutcome {
    Stopped(Stop),
    Exited,
    Blocked,
}

/// What the deadlock detector knows about progress.  "Progress" is any
/// wakeup not caused by a virtual deadline: a blocked thread completing its
/// syscall in the kernel (woken by another thread, I/O, a child), a thread
/// being created or exiting.  Wakeups by deadline (sleeps, timeouts,
/// periodic timers) are counted separately: a JVM, for instance, always has
/// threads waking every few milliseconds, so "every thread is blocked"
/// never holds and a deadlock has to be inferred from nothing else
/// happening for a long time.
#[derive(Clone, Debug, Default)]
pub struct Progress {
    /// Virtual time of the last progress event.
    pub since_ns: u64,
    /// Deadline wakeups since then.
    pub timer_wakes: u64,
    /// Deadline wakeups per thread since then.  A thread that keeps
    /// waking on deadlines is housekeeping; one with a pending deadline
    /// and fewer than `PERIODIC_WAKES` wakeups may be waiting out a
    /// one-shot delay (a 60 s timeout task) that will do something.
    pub timer_woken: BTreeMap<Ltid, u32>,
    /// Threads currently waiting with no deadline, with (timer wakes,
    /// virtual time) when they were first seen waiting; a thread that
    /// stops waiting is removed, so this measures the age of the current
    /// wait.
    pub untimed_since: BTreeMap<Ltid, (u64, u64)>,
}

/// A detected deadlock: the threads waiting with no timeout, the threads
/// that kept waking on timers, and how long nothing else happened.
#[derive(Clone, Debug)]
pub struct DeadlockInfo {
    pub blocked: Vec<Ltid>,
    pub timers: Vec<Ltid>,
    pub vtime_ns: u64,
    pub wakes: u64,
}

impl Session {
    /// Something other than a virtual deadline made a thread runnable.
    pub fn note_progress(&mut self) {
        self.progress.since_ns = self.virtual_ns();
        self.progress.timer_wakes = 0;
        self.progress.timer_woken.clear();
        for tm in &mut self.vtimers {
            tm.fires = 0;
        }
    }

    /// A thread was woken by its virtual deadline.
    pub fn note_timer_wake(&mut self, ltid: Ltid) {
        self.progress.timer_wakes += 1;
        *self.progress.timer_woken.entry(ltid).or_insert(0) += 1;
    }

    /// Called when no thread is runnable: is this a deadlock?  Yes when
    /// some thread waits with no deadline, every thread that does have a
    /// deadline has already woken on it at least once since the last
    /// progress (so no one-shot sleep is still pending), and for
    /// `deadlock_wakes` deadline wakeups and `deadlock_vtime` of virtual
    /// time nothing else happened.
    pub fn deadlock_check(&mut self) -> Option<DeadlockInfo> {
        if !self.opts.virtual_time || self.opts.deadlock_wakes == 0 {
            return None;
        }
        let mut blocked = Vec::new();
        let mut timers = Vec::new();
        let now = self.virtual_ns();
        // A deadline beyond --max-vtime never arrives: the run is a hang
        // before then, so it does not protect against a deadlock verdict.
        let horizon = if self.opts.max_vtime.is_zero() { u64::MAX } else { self.opts.max_vtime.as_nanos() as u64 };
        let mut pending: Vec<(Ltid, u64)> = Vec::new();
        for t in self.alive_tasks() {
            if !matches!(t.state, State::Blocked { .. }) {
                continue;
            }
            let deadline = match t.vblock {
                Some(vb) => vb.deadline,
                None => t.vdeadline.map(|d| d.0),
            };
            match deadline {
                Some(d) => {
                    timers.push(t.ltid);
                    if d <= horizon {
                        pending.push((t.ltid, d));
                    }
                }
                None => blocked.push(t.ltid),
            }
        }
        let wakes = self.progress.timer_wakes;
        let p = &mut self.progress;
        p.untimed_since.retain(|l, _| blocked.contains(l));
        for l in &blocked {
            p.untimed_since.entry(*l).or_insert((wakes, now));
        }
        if blocked.is_empty() {
            return None;
        }
        let need_wakes = self.opts.deadlock_wakes;
        let need_ns = self.opts.deadlock_vtime.as_nanos() as u64;
        // Every untimed wait must be old: seen across `need_wakes` timer
        // wakeups and `need_ns` of virtual time with nothing else
        // happening (progress resets the counters, so a wait that began
        // before the last progress is aged from that point).
        let (w0, t0) = blocked
            .iter()
            .map(|l| p.untimed_since[l])
            .fold((0u64, 0u64), |(w, t), (w1, t1)| (w.max(w1), t.max(t1)));
        let t0 = t0.max(p.since_ns);
        if wakes.saturating_sub(w0) < need_wakes || now.saturating_sub(t0) < need_ns {
            log::debug!(
                "deadlock check: {blocked:?} untimed, {} wakes / {:.1} s since the newest wait or progress (need {need_wakes} / {:.1} s)",
                wakes.saturating_sub(w0),
                now.saturating_sub(t0) as f64 / 1e9,
                need_ns as f64 / 1e9
            );
            return None;
        }
        if let Some(tm) = self.vtimers.iter().find(|tm| {
            tm.deadline.is_some_and(|d| d <= horizon)
                && tm.fires < PERIODIC_WAKES
                && !matches!(tm.kind, crate::vtime::TimerKind::Posix { notify: 1, .. })
        }) {
            log::debug!("deadlock check: a timer ({:?}) is pending and not periodic yet", tm.kind);
            return None;
        }
        if let Some((l, d)) = pending.iter().find(|(l, _)| p.timer_woken.get(l).copied().unwrap_or(0) < PERIODIC_WAKES) {
            log::debug!(
                "deadlock check: T{l} has a deadline at {:.1} s and is not periodic yet (now {:.1} s)",
                *d as f64 / 1e9,
                now as f64 / 1e9
            );
            return None;
        }
        Some(DeadlockInfo {
            blocked,
            timers,
            vtime_ns: now - t0,
            wakes: wakes - w0,
        })
    }

    /// Has the run exceeded `--max-vtime`?  Returns (virtual ns, runnable,
    /// blocked).
    pub fn hang_check(&self) -> Option<(u64, Vec<Ltid>, Vec<Ltid>)> {
        if !self.opts.virtual_time || self.opts.max_vtime.is_zero() {
            return None;
        }
        let now = self.virtual_ns();
        if now < self.opts.max_vtime.as_nanos() as u64 {
            return None;
        }
        Some((now, self.enabled(), self.blocked()))
    }
    /// Spawn `argv[0]` under ptrace and return a session with its main thread
    /// stopped at the syscall-exit of the initial execve (ready to run).
    pub fn spawn(argv: &[String], pmu: PmuConfig, opts: SessionOpts) -> Result<Session> {
        if argv.is_empty() {
            bail!("no command given");
        }
        let cargs: Vec<CString> = argv
            .iter()
            .map(|a| CString::new(a.as_str()).map_err(|e| anyhow!("bad argument: {e}")))
            .collect::<Result<_>>()?;
        let mut argv_ptrs: Vec<*const libc::c_char> = cargs.iter().map(|c| c.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());
        let cenv: Option<Vec<CString>> = opts.env.as_ref().map(|e| {
            e.iter()
                .map(|(k, v)| CString::new(format!("{k}={v}")).unwrap_or_default())
                .collect()
        });
        let envp: Option<Vec<*const libc::c_char>> = cenv.as_ref().map(|e| {
            let mut v: Vec<*const libc::c_char> = e.iter().map(|c| c.as_ptr()).collect();
            v.push(std::ptr::null());
            v
        });
        let ccwd = opts.cwd.as_ref().and_then(|c| CString::new(c.as_str()).ok());

        let mut fds = [0i32; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
            bail!("pipe2: {}", std::io::Error::last_os_error());
        }
        if let Some(ns) = &opts.ns {
            ns.steer_canonical(1)?;
        }
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            bail!("fork: {}", std::io::Error::last_os_error());
        }
        if pid == 0 {
            // Child: only async-signal-safe calls from here.
            unsafe {
                libc::close(fds[1]);
                if opts.ns.is_some() {
                    // /proc must show the pid namespace the program lives
                    // in (getpid() is a namespace pid), so give it a mount
                    // namespace with its own procfs.  Mounting needs the
                    // capabilities of the user namespace, which the exec
                    // below then drops.
                    if libc::unshare(libc::CLONE_NEWNS) == 0 {
                        libc::mount(
                            c"proc".as_ptr(),
                            c"/proc".as_ptr(),
                            c"proc".as_ptr(),
                            0,
                            std::ptr::null(),
                        );
                    }
                }
                libc::personality(libc::ADDR_NO_RANDOMIZE as libc::c_ulong);
                if opts.virtual_time {
                    libc::prctl(
                        libc::PR_SET_TSC,
                        libc::PR_TSC_SIGSEGV as libc::c_ulong,
                        0,
                        0,
                        0,
                    );
                }
                // The program must not inherit our dispositions (the Rust
                // runtime ignores SIGPIPE, ffs ignores what a tracee may
                // broadcast to its process group).
                for sig in 1..=64 {
                    if sig != libc::SIGKILL && sig != libc::SIGSTOP {
                        libc::signal(sig, libc::SIG_DFL);
                    }
                }
                // Make sure the preemption signal is not blocked or ignored.
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
                libc::signal(TIME_SLICE_SIGNAL, libc::SIG_DFL);
                let mut c = 0u8;
                let _ = libc::read(fds[0], &mut c as *mut u8 as *mut libc::c_void, 1);
                libc::close(fds[0]);
                if let Some(c) = &ccwd {
                    libc::chdir(c.as_ptr());
                }
                match &envp {
                    Some(e) => {
                        libc::execvpe(argv_ptrs[0], argv_ptrs.as_ptr(), e.as_ptr());
                    }
                    None => {
                        libc::execvp(argv_ptrs[0], argv_ptrs.as_ptr());
                    }
                }
                let msg = b"ffs: exec failed\n";
                libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
                libc::_exit(127);
            }
        }
        unsafe { libc::close(fds[0]) };
        ptrace::seize(pid, ptrace::SEIZE_OPTIONS).context("seize initial child")?;
        if unsafe { libc::write(fds[1], b"g".as_ptr() as *const libc::c_void, 1) } != 1 {
            bail!("write to sync pipe failed");
        }
        unsafe { libc::close(fds[1]) };

        let vrandom = VirtualRandom::new(opts.seed);
        let first_port = FIRST_PORT + ((opts.seed % 200) as u16) * 50;
        let mut session = Session {
            cpu_online: None,
            tasks: BTreeMap::new(),
            ltids: BTreeMap::new(),
            next_ltid: 1,
            pmu,
            opts,
            vclock: VirtualClock {
                base_realtime_ns: 1_700_000_000_000_000_000,
                base_monotonic_ns: 1_000_000_000,
                events: 0,
                skew: 0,
            },
            main_tgid: pid,
            zombies: Vec::new(),
            logs: Vec::new(),
            vrandom,
            replay: false,
            expected_signal: None,
            vtimers: Vec::new(),
            siginfo_queue: Vec::new(),
            parent_of: BTreeMap::new(),
            vzombies: Vec::new(),
            stopped_procs: Default::default(),
            jobstops: Vec::new(),
            jobconts: Vec::new(),
            pdeathsig: BTreeMap::new(),
            exit_override: BTreeMap::new(),
            exited_procs: std::collections::BTreeSet::new(),
            socket_peers: BTreeMap::new(),
            watchpoints: Vec::new(),
            watch_requests: Vec::new(),
            watch_pending: Default::default(),
            watch_hits: [0; 4],
            watch_disabled: 0,
            file_snapshots: BTreeMap::new(),
            watch_preempt: false,
            progress: Progress::default(),
            created_at: BTreeMap::new(),
            first_output: None,
            prearmed: std::collections::BTreeSet::new(),
            ports_used: Vec::new(),
            port_plan: None,
            // Concurrent sessions on one host start in different parts of
            // the range (a replay follows its recorded plan anyway).
            next_port: first_port,
            cap_preempt: false,
        };
        // Wait for the exec event.  Before it the child runs our own code.
        loop {
            let st = ptrace::wait_tid(pid)?.ok_or_else(|| anyhow!("child vanished before exec"))?;
            match st {
                WaitStatus::Event(libc::PTRACE_EVENT_EXEC) => break,
                WaitStatus::Exited(c) => bail!("child exited with {c} before exec"),
                WaitStatus::Signaled(s) => bail!("child killed by signal {s} before exec"),
                WaitStatus::SyscallStop | WaitStatus::Event(_) | WaitStatus::GroupStop(_) => {
                    ptrace::resume(pid, ResumeHow::Cont, 0)?;
                }
                WaitStatus::Signal(sig) => {
                    ptrace::resume(pid, ResumeHow::Cont, sig)?;
                }
            }
        }
        let mut t = Task::new(pid, pid, 1, &session.pmu)?;
        t.execs = 1;
        t.state = State::Stopped(Stop::Parked);
        // The execve syscall is still in progress: consume its exit stop
        // now, so that the task is in an ordinary stop (a checkpoint can be
        // taken right away) and report it as the first event.
        t.in_syscall = true;
        t.syscall_nr = EXECVE;
        session.ltids.insert(1, pid);
        session.next_ltid = 2;
        session.tasks.insert(pid, t);
        session.post_exec(pid)?;
        session.check_ns_pid(pid, 1);
        {
            let t = session.task_mut(pid)?;
            t.resume(ResumeHow::Syscall, 0)?;
            loop {
                let st = ptrace::wait_tid(pid)?.ok_or_else(|| anyhow!("child vanished at exec"))?;
                match t.did_wait(st)? {
                    Some(Stop::SyscallExit { .. }) => break,
                    Some(Stop::Parked) => t.resume(ResumeHow::Syscall, 0)?,
                    Some(other) => bail!("unexpected stop {other:?} after exec"),
                    None => bail!("child died at exec"),
                }
            }
            t.unreported = true;
        }
        Ok(session)
    }

    pub fn task(&self, tid: pid_t) -> Result<&Task> {
        self.tasks
            .get(&tid)
            .ok_or_else(|| anyhow!("unknown task {tid}"))
    }

    pub fn task_mut(&mut self, tid: pid_t) -> Result<&mut Task> {
        self.tasks
            .get_mut(&tid)
            .ok_or_else(|| anyhow!("unknown task {tid}"))
    }

    pub fn tid_of(&self, ltid: Ltid) -> Result<pid_t> {
        self.ltids
            .get(&ltid)
            .copied()
            .ok_or_else(|| anyhow!("unknown logical thread {ltid}"))
    }

    pub fn by_ltid(&mut self, ltid: Ltid) -> Result<&mut Task> {
        let tid = self.tid_of(ltid)?;
        self.task_mut(tid)
    }

    pub fn alive_tasks(&self) -> impl Iterator<Item = &Task> {
        self.tasks.values().filter(|t| t.is_alive())
    }

    pub fn all_exited(&self) -> bool {
        self.tasks.values().all(|t| !t.is_alive())
    }

    /// Total virtual nanoseconds elapsed (sum of ticks over all threads).
    pub fn virtual_ns(&self) -> u64 {
        let ticks: u64 = self.tasks.values().map(|t| t.ticks).sum();
        ticks + self.vclock.events * 1000 + self.vclock.skew
    }

    /// Exit code of the main process, if it exited.
    /// No thread of the main process is alive any more.
    pub fn main_process_exited(&self) -> bool {
        self.main_exit_code().is_some()
            && !self.tasks.values().any(|t| t.tgid == self.main_tgid && t.is_alive())
    }

    pub fn main_exit_code(&self) -> Option<i32> {
        self.tasks.get(&self.main_tgid).and_then(|t| t.exit_code)
    }

    fn post_exec(&mut self, tid: pid_t) -> Result<()> {
        let virtual_time = self.opts.virtual_time;
        let t = self.task_mut(tid)?;
        // /proc/<pid>/mem is bound to the address space it was opened on;
        // exec gave the task a new one.
        t.mem = Mem::open(tid);
        t.invalidate_regs();
        t.vdso_patched = false;
        t.rseq = None;
        self.watch_new_task(tid);
        if self.opts.virtual_random {
            if let Err(e) = self.random_exec(tid) {
                log::warn!("could not virtualize AT_RANDOM for task {tid}: {e:#}");
            }
        }
        let t = self.task_mut(tid)?;
        if virtual_time {
            match vdso::patch_vdso(&t.mem) {
                Ok(n) => {
                    t.vdso_patched = true;
                    log::debug!("patched {n} vDSO entry points in task {tid}");
                }
                Err(e) => log::warn!("could not patch vDSO for task {tid}: {e:#}"),
            }
        }
        Ok(())
    }

    /// Register a thread/process created by `parent` via clone/fork.
    fn register_child(&mut self, parent: pid_t, child: pid_t) -> Result<Ltid> {
        // Wait for the child's initial stop before touching it.
        let initial = match ptrace::wait_tid(child)? {
            Some(WaitStatus::Event(ptrace::PTRACE_EVENT_STOP))
            | Some(WaitStatus::GroupStop(_))
            | Some(WaitStatus::Signal(libc::SIGSTOP)) => Some(()),
            Some(WaitStatus::Exited(_)) | Some(WaitStatus::Signaled(_)) | None => None,
            Some(other) => {
                log::warn!("unexpected initial status {other:?} for new task {child}");
                Some(())
            }
        };
        let tgid = proc_tgid(child).unwrap_or(child);
        let ltid = self.next_ltid;
        self.next_ltid += 1;
        let mut t = Task::new(child, tgid, ltid, &self.pmu)?;
        let parent_task = self.task(parent)?;
        t.vdso_patched = parent_task.vdso_patched;
        t.tsc_mode = parent_task.tsc_mode;
        if initial.is_none() {
            t.state = State::Exited;
        }
        self.ltids.insert(ltid, child);
        self.tasks.insert(child, t);
        let now = self.virtual_ns();
        self.created_at.insert(ltid, now);
        self.note_progress();
        log::debug!("new task tid={child} tgid={tgid} ltid={ltid} (parent {parent})");
        self.check_ns_pid(child, ltid);
        if initial.is_some() {
            self.watch_new_task(child);
        }
        Ok(ltid)
    }

    /// Verify that a thread got its canonical id inside the pid namespace.
    pub fn check_ns_pid(&self, tid: pid_t, ltid: Ltid) {
        if self.opts.ns.is_none() {
            return;
        }
        let want = ns::canonical_tid(ltid);
        match ns::ns_pid(tid) {
            Some(got) if got == want => {}
            got => log::warn!("T{ltid} got pid {got:?} in the namespace instead of {want}; ids are not stable"),
        }
    }

    /// Wait for `tid` (which we resumed) to stop, exit, or block in the kernel.
    pub fn wait_task(&mut self, tid: pid_t, allow_block: bool) -> Result<WaitOutcome> {
        let mut sleeping_since: Option<Instant> = None;
        let started = Instant::now();
        loop {
            if TERMINATED.load(std::sync::atomic::Ordering::SeqCst) {
                bail!("terminated by SIGTERM");
            }
            if let Some(st) = ptrace::try_wait_tid(tid)? {
                let t = self.task_mut(tid)?;
                return Ok(match t.did_wait(st)? {
                    Some(stop) => WaitOutcome::Stopped(stop),
                    None => WaitOutcome::Exited,
                });
            }
            // Sleep until the kernel tells us about a stop (SIGCHLD is sent
            // to the tracer for every ptrace stop) rather than polling; the
            // timeout bounds how long a syscall may run before we look at
            // whether it is blocked.
            let grace = if self.replay { self.opts.deadlock_timeout } else { self.opts.block_grace };
            let timeout = if allow_block {
                let since = *sleeping_since.get_or_insert_with(Instant::now);
                grace.saturating_sub(since.elapsed()).max(Duration::from_micros(20))
            } else {
                Duration::from_millis(2)
            };
            ptrace::wait_sigchld(timeout);
            if let Some(st) = ptrace::try_wait_tid(tid)? {
                let t = self.task_mut(tid)?;
                return Ok(match t.did_wait(st)? {
                    Some(stop) => WaitOutcome::Stopped(stop),
                    None => WaitOutcome::Exited,
                });
            }
            if !allow_block && !self.task(tid)?.in_syscall && !self.replay
                && !self.opts.slice_cap.is_zero() && started.elapsed() >= self.opts.slice_cap {
                // User code without ticks (a loop of unconditional jumps):
                // interrupt it and make that a preemption, which a replay
                // reaches by breakpoint like any other.
                ptrace::interrupt(tid)?;
                let t = self.task_mut(tid)?;
                t.interrupt_pending = true;
                let Some(st) = ptrace::wait_tid(tid)? else {
                    return Ok(WaitOutcome::Exited);
                };
                self.cap_preempt = true;
                let t = self.task_mut(tid)?;
                return Ok(match t.did_wait(st)? {
                    Some(Stop::Parked) => WaitOutcome::Stopped(Stop::Signal { sig: TIME_SLICE_SIGNAL }),
                    Some(stop) => WaitOutcome::Stopped(stop),
                    None => WaitOutcome::Exited,
                });
            }
            match ptrace::proc_state(tid) {
                Some('S') | Some('D') if allow_block || sleeping_since.is_some_and(|s| s.elapsed() >= self.opts.deadlock_timeout) => {
                    // Blocked in the kernel.  Give it a grace period so that
                    // syscalls that merely sleep briefly are not misclassified.
                    // A thread that was not expected to block (resumed
                    // through user code, `allow_block` false) but sleeps for
                    // as long as a deadlock is reported blocked too, so a
                    // replay that went wrong reports a divergence instead of
                    // waiting forever.
                    let since = *sleeping_since.get_or_insert_with(Instant::now);
                    // In a replay the trace says whether the syscall blocks
                    // (a `Blocked` step is replayed without waiting), so a
                    // syscall that is waited for is one that completed; it
                    // gets as long as a deadlock would rather than letting
                    // disk latency under load reclassify it.
                    if since.elapsed() >= grace {
                        let t = self.task_mut(tid)?;
                        let delta = t.hpc.read()?;
                        // Keep counting: ticks are accumulated when it stops.
                        let _ = delta;
                        let nr = t.syscall_nr;
                        t.state = State::Blocked { nr };
                        return Ok(WaitOutcome::Blocked);
                    }
                }
                Some('Z') | Some('X') | None => {
                    // Dead but not reapable yet (zombie thread-group leader).
                    if let Some(st) = ptrace::try_wait_tid(tid)? {
                        let t = self.task_mut(tid)?;
                        return Ok(match t.did_wait(st)? {
                            Some(stop) => WaitOutcome::Stopped(stop),
                            None => WaitOutcome::Exited,
                        });
                    }
                    let t = self.task_mut(tid)?;
                    let _ = t.hpc.stop();
                    t.state = State::Exited;
                    if t.exit_code.is_none() {
                        t.exit_code = Some(0);
                    }
                    self.zombies.push(tid);
                    return Ok(WaitOutcome::Exited);
                }
                Some('S') | Some('D') => {
                    sleeping_since.get_or_insert_with(Instant::now);
                }
                _ => {
                    sleeping_since = None;
                }
            }
        }
    }

    /// Poll blocked tasks and pick up any that completed.  Also notices
    /// asynchronous state changes (e.g. a stopped thread killed by exit_group).
    pub fn settle(&mut self) -> Result<()> {
        // One sweep for every status the kernel has for us, then a lookup
        // per task: a thread that completed its blocking syscall, or was
        // killed, reports a stop or an exit here.
        ptrace::drain_statuses()?;
        let tids: Vec<pid_t> = self.tasks.values().filter(|t| t.is_alive()).map(|t| t.tid).collect();
        for tid in tids {
            let Some(st) = ptrace::take_stashed(tid) else { continue };
            let state = self.task(tid)?.state;
            match state {
                State::Blocked { .. } => {
                    let t = self.task_mut(tid)?;
                    // Parked by us (virtual block) or blocked in the kernel:
                    // either way it now has a real status.
                    t.vblock = None;
                    t.vwait = None;
                    if t.did_wait(st)?.is_some() {
                        t.unreported = true;
                    }
                    // Woken in the kernel by someone (or killed).
                    self.note_progress();
                }
                State::Stopped(_) => {
                    // Asynchronous change (SIGKILL / exit_group).
                    let t = self.task_mut(tid)?;
                    if t.did_wait(st)?.is_some() {
                        t.unreported = true;
                    }
                }
                State::Running => {
                    // Not ours to consume here (someone is waiting for it).
                    ptrace::stash(tid, st);
                }
                State::Exited => {}
            }
        }
        self.reap_zombies();
        self.proc_check_stopped()?;
        self.vt_check()?;
        Ok(())
    }

    pub fn reap_zombies(&mut self) {
        let mut reaped = Vec::new();
        self.zombies.retain(|&tid| match ptrace::try_wait_tid(tid) {
            Ok(Some(st)) => {
                reaped.push((tid, st));
                false
            }
            _ => true,
        });
        for (tid, st) in reaped {
            if let Some(t) = self.tasks.get_mut(&tid) {
                match st {
                    WaitStatus::Exited(code) => {
                        t.exit_code = Some(code);
                        t.exit_status = Some((code & 0xff) << 8);
                    }
                    WaitStatus::Signaled(sig) => {
                        t.exit_code = Some(128 + sig);
                        t.exit_status = Some(sig);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Is a thread we consider blocked actually running in the kernel on
    /// its way to a ptrace stop (just woken)?  Waits for such threads to
    /// arrive so that an idle jump of the virtual clock is not taken while
    /// a wakeup is in flight.  The bound only matters on a machine so
    /// loaded that a woken thread needs that long to reach its stop; a
    /// replay, which knows the wakeup is coming, waits as long as for a
    /// deadlock.
    fn wait_in_flight(&mut self) -> Result<bool> {
        let start = Instant::now();
        let bound = if self.replay { self.opts.deadlock_timeout } else { Duration::from_millis(500) };
        loop {
            let in_flight: Vec<pid_t> = self
                .tasks
                .values()
                .filter(|t| matches!(t.state, State::Blocked { .. }) && t.vblock.is_none())
                .filter(|t| matches!(ptrace::proc_state(t.tid), Some('R') | Some('t') | Some('T')))
                .map(|t| t.tid)
                .collect();
            if in_flight.is_empty() {
                return Ok(false);
            }
            if start.elapsed() >= bound {
                return Ok(true);
            }
            std::thread::sleep(Duration::from_micros(20));
            self.settle()?;
            if !self.enabled().is_empty() {
                return Ok(true);
            }
        }
    }

    /// Logical ids of tasks that are stopped (can be resumed now), in ltid order.
    pub fn enabled(&self) -> Vec<Ltid> {
        // Threads of a process in an emulated group stop are not runnable.
        let stopped: Vec<pid_t> = self.stopped_procs.iter().filter_map(|k| self.ltids.get(k).copied()).collect();
        let mut v: Vec<Ltid> = self
            .tasks
            .values()
            .filter(|t| t.is_stopped() && !stopped.contains(&t.tgid))
            .map(|t| t.ltid)
            .collect();
        v.sort_unstable();
        v
    }

    pub fn blocked(&self) -> Vec<Ltid> {
        let mut v: Vec<Ltid> = self
            .tasks
            .values()
            .filter(|t| matches!(t.state, State::Blocked { .. }))
            .map(|t| t.ltid)
            .collect();
        v.sort_unstable();
        v
    }

    /// Replay: wait (real time) for a blocked thread's syscall to complete,
    /// without jumping the virtual clock.  Returns false on timeout.
    pub fn wait_for_completion(&mut self) -> Result<bool> {
        let start = Instant::now();
        loop {
            self.settle()?;
            if !self.enabled().is_empty() || self.all_exited() {
                return Ok(true);
            }
            if start.elapsed() > self.opts.deadlock_timeout {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_micros(100));
        }
    }

    /// Block until at least one task is enabled, or the deadlock timeout
    /// expires.  Returns false on timeout.
    pub fn wait_for_any_enabled(&mut self) -> Result<bool> {
        let start = Instant::now();
        let mut advances = 0u32;
        loop {
            self.settle()?;
            if !self.enabled().is_empty() || self.all_exited() {
                return Ok(true);
            }
            // A wakeup in flight is not idleness.
            if self.wait_in_flight()? {
                continue;
            }
            // Virtual time: jump to the next deadline instead of waiting.
            if advances < 100_000 && self.vt_advance_idle()? {
                advances += 1;
                continue;
            }
            if start.elapsed() > self.opts.deadlock_timeout {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    /// Run the task with logical id `ltid` until its next scheduler-visible
    /// event.  `budget` is the tick budget (0 = unlimited) used when running
    /// user code.
    pub fn run_task(&mut self, ltid: Ltid, budget: u64) -> Result<Event> {
        let tid = self.tid_of(ltid)?;
        self.vclock.events += 1;
        self.watch_preempt = false;
        let replay = self.replay;
        loop {
            let t = self.task_mut(tid)?;
            match t.state {
                State::Stopped(stop) if t.unreported => {
                    // Reached asynchronously (completed a blocking syscall,
                    // or killed by exit_group): report it without resuming.
                    t.unreported = false;
                    if let Some(ev) = self.handle_stop(tid, stop)? {
                        return Ok(ev);
                    }
                    continue;
                }
                State::Stopped(_) => {}
                State::Blocked { .. } if t.vblock.is_some() || t.vdeadline.is_some() => {
                    // Blocked on virtual time: it can only run if its
                    // deadline has passed (a replay following the trace).
                    self.vt_poll_task(tid)?;
                    let t = self.task(tid)?;
                    if let Some(vb) = t.vblock {
                        bail!("T{ltid} is sleeping until virtual time {:?} (now {}); the schedule cannot be followed", vb.deadline, self.virtual_ns());
                    }
                    if !t.is_stopped() {
                        // Blocked in the kernel with a deadline not yet
                        // reached: it may still complete for real.
                        self.note_progress();
                        match self.wait_task(tid, false)? {
                            WaitOutcome::Exited => {
                                let code = self.task(tid)?.exit_code.unwrap_or(0);
                                return Ok(Event::Exited { code });
                            }
                            WaitOutcome::Blocked => {
                            // Not expected to block here (a replay that no
                            // longer follows its trace): report it.
                            let nr = self.task(tid)?.syscall_nr;
                            return Ok(Event::Blocked { nr });
                        }
                            WaitOutcome::Stopped(stop) => {
                                if let Some(ev) = self.handle_stop(tid, stop)? {
                                    return Ok(ev);
                                }
                            }
                        }
                    }
                    continue;
                }
                State::Blocked { .. } => {
                    // The scheduler chose a blocked task: wait for it.
                    match self.wait_task(tid, false)? {
                        WaitOutcome::Exited => {
                            let code = self.task(tid)?.exit_code.unwrap_or(0);
                            return Ok(Event::Exited { code });
                        }
                        WaitOutcome::Blocked => {
                            // Not expected to block here (a replay that no
                            // longer follows its trace): report it.
                            let nr = self.task(tid)?.syscall_nr;
                            return Ok(Event::Blocked { nr });
                        }
                        WaitOutcome::Stopped(stop) => {
                            if let Some(ev) = self.handle_stop(tid, stop)? {
                                return Ok(ev);
                            }
                        }
                    }
                    continue;
                }
                State::Running => bail!("task {tid} is running; serial execution violated"),
                State::Exited if replay => {
                    // A thread that is already gone (the process was killed
                    // from outside): the trace has `exiting` then `exited`.
                    if !t.exit_reported {
                        t.exit_reported = true;
                        return Ok(Event::Exit);
                    }
                    let code = t.exit_code.unwrap_or(0);
                    return Ok(Event::Exited { code });
                }
                State::Exited => bail!("task {tid} has exited"),
            }
            // Resume from the current stop.  No tick budget is armed when
            // entering a syscall: no user code runs before the exit stop, and
            // an overflow interrupt landing inside the kernel would abort the
            // syscall with ERESTARTSYS.
            let stop = self.task(tid)?.stop().unwrap();
            let mut how = ResumeHow::Syscall;
            let mut allow_block = false;
            let mut budget = budget;
            match stop {
                Stop::SyscallEntry { nr } => {
                    allow_block = crate::effects::may_block(self.task_mut(tid)?, nr)?;
                    budget = 0;
                    self.pre_syscall(tid, nr)?;
                }
                Stop::CloneEvent { .. } => {
                    // Still inside clone: a vfork parent now sleeps until
                    // the child execs or exits, so it may block here.
                    allow_block = true;
                    budget = 0;
                }
                Stop::ExitEvent => how = ResumeHow::Cont,
                _ if replay => {}
                _ => {
                    // A virtual deadline (timer, another thread's sleep)
                    // that falls inside this slice ends the slice there, so
                    // timers fire at a deterministic point in user code.
                    // (A replay has those points in the trace and arms no
                    // budget: a late overflow could race the next syscall.)
                    if let Some(d) = self.vt_next_deadline() {
                        let left = d.saturating_sub(self.virtual_ns()).max(1);
                        if budget == 0 || left < budget {
                            budget = left;
                        }
                    }
                }
            }
            if let Err(e) = self.task_mut(tid)?.resume(how, budget) {
                // The thread died under us (its process was killed): collect
                // its status and report the exit.
                if format!("{e:#}").contains("os error 3") {
                    let t = self.task_mut(tid)?;
                    if let Some(st) = ptrace::wait_tid(tid)? {
                        t.did_wait(st)?;
                    } else {
                        t.state = State::Exited;
                    }
                    t.exit_reported = true;
                    return Ok(Event::Exit);
                }
                return Err(e);
            }
            match self.wait_task(tid, allow_block)? {
                WaitOutcome::Exited => {
                    let code = self.task(tid)?.exit_code.unwrap_or(0);
                    return Ok(Event::Exited { code });
                }
                WaitOutcome::Blocked => {
                    let nr = self.task(tid)?.syscall_nr;
                    return Ok(Event::Blocked { nr });
                }
                WaitOutcome::Stopped(stop) => {
                    if let Some(ev) = self.handle_stop(tid, stop)? {
                        return Ok(ev);
                    }
                }
            }
        }
    }

    /// Convert a stop into a visible event, performing the side effects of
    /// internal stops (clone/exec bookkeeping, magic-call return values,
    /// virtual time, rdtsc emulation).  Returns None for internal stops.
    pub fn handle_stop(&mut self, tid: pid_t, stop: Stop) -> Result<Option<Event>> {
        if self.watch_pending.remove(&tid) {
            self.watch_new_task(tid);
        }
        match stop {
            // Spurious PTRACE_INTERRUPT stop or group-stop: nothing happened.
            Stop::Parked => Ok(None),
            Stop::SyscallEntry { nr } => {
                let t = self.task_mut(tid)?;
                if t.restarting {
                    // Re-entry of a syscall the kernel restarted after the
                    // preemption signal interrupted it: not a new event.
                    t.restarting = false;
                    // Preserve a stripped virtual deadline on re-entry.
                    t.rewound = true;
                    if nr == RESTART_SYSCALL {
                        // A timed wait (poll, nanosleep, futex with a
                        // timeout) is restarted through restart_syscall;
                        // the exit still belongs to the original call.
                        let orig = t.syscall_nr;
                        t.state = State::Stopped(Stop::SyscallEntry { nr: orig });
                    }
                    return Ok(None);
                }
                if let Some(call) = ffs_api::decode(nr) {
                    return Ok(Some(self.magic_event(tid, call)?));
                }
                Ok(Some(Event::SyscallEntry { nr }))
            }
            Stop::SyscallExit { nr, .. } if self.task(tid)?.emulated.is_some() => {
                let em = self.task_mut(tid)?.emulated.take().unwrap();
                Ok(Some(self.vt_emulated_exit(tid, nr, em)?))
            }
            Stop::SyscallExit { ret, .. }
                if ret == -(libc::EINTR as i64)
                    && ptrace::pending_signals(tid) & (1u64 << (TIME_SLICE_SIGNAL - 1)) != 0
                    && ptrace::interrupting_signals(tid) == 0 =>
            {
                // A late PMU overflow can arrive after syscall entry.
                // Non-restartable waits (notably epoll) return EINTR for
                // it. Make the kernel restart after we suppress our signal,
                // rather than exposing an interruption from the tracer.
                let t = self.task_mut(tid)?;
                let mut r = t.regs()?;
                r.rax = (-512i64) as u64; // ERESTARTSYS
                t.set_regs(&r)?;
                t.restarting = true;
                Ok(None)
            }
            Stop::SyscallExit { nr, ret } if ptrace::is_restart_result(ret as u64) => {
                // Interrupted by a signal; the kernel will restart it once the
                // signal has been dealt with.  If the signal turns out to be
                // our own preemption signal the whole thing is invisible.
                let _ = nr;
                self.task_mut(tid)?.restarting = true;
                Ok(None)
            }
            Stop::SyscallExit { nr: BIND, ret }
                if ret == -(libc::EADDRINUSE as i64)
                    && self.task(tid)?.bound_port.is_some()
                    && self.task(tid)?.port_tries < 200
                    && !self.port_planned() =>
            {
                // The host has our port in use: try the next one, with the
                // syscall re-executed invisibly (as a kernel restart is).
                let p = self.fresh_port();
                if let Some(last) = self.ports_used.last_mut() {
                    *last = p;
                }
                let t = self.task_mut(tid)?;
                let addr = t.mem_restore.as_ref().map(|(a, _)| *a).unwrap_or(0);
                t.mem.write(addr, &p.to_be_bytes())?;
                t.bound_port = Some(p);
                t.port_tries += 1;
                let mut r = t.regs()?;
                r.rip -= ptrace::SYSCALL_INSN_LEN;
                r.rax = BIND;
                r.orig_rax = BIND;
                t.set_regs(&r)?;
                t.in_syscall = false;
                t.restarting = true;
                t.rewound = true;
                t.state = State::Stopped(Stop::Parked);
                Ok(None)
            }
            Stop::SyscallExit { nr, ret: _ } => {
                if self.task(tid)?.entry_regs.is_some() || self.task(tid)?.vdeadline.is_some() || self.task(tid)?.mem_restore.is_some() {
                    self.vt_syscall_done(tid)?;
                }
                if ffs_api::decode(nr).is_some() {
                    // Fix the return value of a magic call and keep going.
                    let fixed = ffs_api::return_value(nr);
                    let t = self.task_mut(tid)?;
                    let mut r = t.regs()?;
                    r.rax = fixed as u64;
                    t.set_regs(&r)?;
                    return Ok(None);
                }
                if self.opts.virtual_time {
                    self.virtualize_time_result(tid, nr)?;
                }
                if self.opts.virtual_random {
                    self.virtualize_random_result(tid, nr)?;
                }
                // The event carries the value the program sees, which the
                // virtualization above may have changed.
                let ret = self.task_mut(tid)?.regs()?.rax as i64;
                if nr == SOCKETPAIR && ret == 0 {
                    // Remember which sockets are peers, so that a checkpoint
                    // can recreate the pair.
                    let t = self.task_mut(tid)?;
                    let r = t.regs()?;
                    let mut sv = [0u8; 8];
                    if t.mem.read(r.r10, &mut sv).is_ok() {
                        let a = i32::from_ne_bytes(sv[..4].try_into().unwrap());
                        let b = i32::from_ne_bytes(sv[4..].try_into().unwrap());
                        if let (Some(ia), Some(ib)) = (socket_inode(tid, a), socket_inode(tid, b)) {
                            self.socket_peers.insert(ia, ib);
                            self.socket_peers.insert(ib, ia);
                        }
                    }
                }
                if nr == PRCTL && ret == 0 {
                    // The parent death signal is sent by the emulated
                    // process tree (the real parent is ffs).
                    const PR_SET_PDEATHSIG: u64 = 1;
                    let t = self.task_mut(tid)?;
                    let r = t.regs()?;
                    if r.rdi == PR_SET_PDEATHSIG {
                        let key = self.proc_key(tid)?;
                        if r.rsi == 0 {
                            self.pdeathsig.remove(&key);
                        } else {
                            self.pdeathsig.insert(key, r.rsi as i32);
                        }
                    }
                }
                if nr == RSEQ && ret == 0 {
                    let t = self.task_mut(tid)?;
                    let r = t.regs()?;
                    t.rseq = if r.rdx & RSEQ_FLAG_UNREGISTER != 0 {
                        None
                    } else {
                        Some((r.rdi, r.rsi, r.r10))
                    };
                }
                if matches!(nr, KILL | TKILL | TGKILL | RT_SIGQUEUEINFO | RT_TGSIGQUEUEINFO | PIDFD_SEND_SIGNAL) {
                    ptrace::note_signal_sent();
                }
                if self.first_output.is_none() && matches!(nr, WRITE | WRITEV) && ret > 0 {
                    let fd = self.task_mut(tid)?.regs()?.rdi;
                    if fd == 1 || fd == 2 {
                        self.first_output = Some(self.virtual_ns());
                    }
                }
                let child = self.task_mut(tid)?.last_child.take();
                Ok(Some(Event::SyscallExit { nr, ret, child }))
            }
            Stop::Signal { sig } => {
                if sig == TIME_SLICE_SIGNAL {
                    // Preemption: the signal is suppressed.
                    let t = self.task_mut(tid)?;
                    t.pending_sig = 0;
                    if t.restarting {
                        // Arrived while inside a syscall: let the kernel
                        // restart the syscall; the preemption is dropped.
                        return Ok(None);
                    }
                    if std::mem::replace(&mut self.cap_preempt, false) {
                        self.vclock.skew += self.opts.slice_cap.as_nanos() as u64;
                        return Ok(Some(Event::Slice));
                    }
                    return Ok(Some(Event::Preempt));
                }
                if self.task(tid)?.restarting {
                    // A real signal interrupted the syscall; after the handler
                    // the kernel may restart it, which then shows up as a new
                    // syscall entry.
                    self.task_mut(tid)?.restarting = false;
                }
                if sig == libc::SIGSEGV && self.opts.virtual_time && self.try_emulate_rdtsc(tid)? {
                    return Ok(None);
                }
                if sig == libc::SIGTRAP {
                    // A trap-flag trap the tracer did not ask for: the
                    // trap flag leaked into the program (see
                    // `Task::after_single_step`).  Clear it and carry on.
                    let t = self.task_mut(tid)?;
                    let mut r = t.regs()?;
                    if r.eflags & crate::task::X86_TF != 0
                        && ptrace::get_siginfo(tid).map(|i| ptrace::siginfo_code(&i) == TRAP_TRACE).unwrap_or(false)
                    {
                        log::debug!("task {tid}: spurious single-step trap at {:#x}, clearing TF", r.rip);
                        r.eflags &= !crate::task::X86_TF;
                        t.set_regs(&r)?;
                        t.pending_sig = 0;
                        t.state = State::Stopped(Stop::Parked);
                        return Ok(None);
                    }
                }
                if sig == libc::SIGTRAP && !self.watchpoints.is_empty() {
                    if let Ok(i) = ptrace::get_siginfo(tid) {
                        if ptrace::siginfo_code(&i) == crate::watch::TRAP_HWBKPT {
                            // An access to a watched variable: a scheduling
                            // point, recorded as a preemption.  Replays reach
                            // the same point by tick count instead.
                            let bits = crate::watch::watchpoint_hits(tid);
                            let t = self.task_mut(tid)?;
                            t.pending_sig = 0;
                            t.state = State::Stopped(Stop::Parked);
                            self.watch_preempt = true;
                            self.watch_account(bits)?;
                            return Ok(if self.replay { None } else { Some(Event::Preempt) });
                        }
                    }
                }
                // A signal the tracer sent for an emulated kernel facility
                // (timer, SIGCHLD): install the siginfo the kernel would
                // have produced.
                let ltid = self.task(tid)?.ltid;
                let mut ours = false;
                if let Some(i) = self.siginfo_queue.iter().position(|(l, s, _)| *l == ltid && *s == sig) {
                    let (_, _, info) = self.siginfo_queue.remove(i);
                    ptrace::set_siginfo(tid, &info)?;
                    ours = true;
                }
                let raw = ptrace::get_siginfo(tid).ok();
                // A signal the tracer generated itself (virtual timer,
                // emulated SIGCHLD) is part of the schedule, not external.
                if self.replay && !ours && self.expected_signal != Some(sig) {
                    if let Some(i) = &raw {
                        if is_external_async(sig, i) {
                            log::debug!("replay: discarding unexpected signal {sig} for task {tid}");
                            let t = self.task_mut(tid)?;
                            t.pending_sig = 0;
                            t.state = State::Stopped(Stop::Parked);
                            return Ok(None);
                        }
                    }
                }
                let info = raw.map(|i| ptrace::siginfo_to_hex(&i)).unwrap_or_default();
                // A fault in the init of a pid namespace: the kernel ignores
                // a signal a tracer injects into an init (only a forced one
                // would kill it), so the instruction would fault forever.
                // Kill it ourselves and let its parent see the real status.
                let synchronous = matches!(sig, libc::SIGSEGV | libc::SIGBUS | libc::SIGILL | libc::SIGFPE)
                    && raw.map(|i| ptrace::siginfo_code(&i) > 0).unwrap_or(false);
                if synchronous
                    && crate::ns::ns_pid(tid) == Some(1)
                    && (ptrace::caught_signals(tid) | ptrace::ignored_signals(tid)) & (1u64 << (sig - 1)) == 0
                {
                    let tgid = self.task(tid)?.tgid;
                    self.exit_override.insert(tgid, sig | 0x80);
                    unsafe {
                        libc::kill(tgid, libc::SIGKILL);
                    }
                    let t = self.task_mut(tid)?;
                    t.pending_sig = 0;
                    t.state = State::Stopped(Stop::Parked);
                    return Ok(Some(Event::Signal { sig, info }));
                }
                // Job control: a stop signal with the default action stops
                // the process (emulated: it is not scheduled until SIGCONT);
                // SIGCONT continues a stopped one.  Either is a scheduling
                // event recorded like any signal.
                let key = self.proc_key(tid)?;
                if matches!(sig, libc::SIGSTOP | libc::SIGTSTP | libc::SIGTTIN | libc::SIGTTOU) {
                    let bit = 1u64 << (sig - 1);
                    let dfl = sig == libc::SIGSTOP
                        || (ptrace::caught_signals(tid) | ptrace::ignored_signals(tid)) & bit == 0;
                    if dfl {
                        let t = self.task_mut(tid)?;
                        t.pending_sig = 0;
                        t.state = State::Stopped(Stop::Parked);
                        self.on_process_stop(key, sig)?;
                        return Ok(Some(Event::Signal { sig, info }));
                    }
                } else if sig == libc::SIGCONT && self.stopped_procs.contains(&key) {
                    self.on_process_cont(key, true)?;
                }
                self.task_mut(tid)?.pending_sig = sig;
                Ok(Some(Event::Signal { sig, info }))
            }
            Stop::CloneEvent { child, .. } => {
                // Consult the application's flags, before our CLONE_PARENT
                // rewrite, to preserve sibling creation in the logical tree.
                let parent = self.task_mut(tid)?;
                let r = parent.entry_regs.unwrap_or(parent.regs()?);
                let sibling = match parent.syscall_nr {
                    56 => r.rdi & libc::CLONE_PARENT as u64 != 0,
                    435 => {
                        let flags = match &parent.mem_restore {
                            Some((addr, bytes)) if *addr == r.rdi && bytes.len() == 8 =>
                                u64::from_ne_bytes(bytes.as_slice().try_into().unwrap()),
                            _ => parent.mem.read_u64(r.rdi)?,
                        };
                        flags & libc::CLONE_PARENT as u64 != 0
                    }
                    _ => false,
                };
                let ltid = self.register_child(tid, child)?;
                self.task_mut(tid)?.last_child = Some(ltid);
                // fork/vfork were rewritten into clone with CLONE_PARENT:
                // the parent gets its argument registers back at the exit
                // stop, the child must get them back here (glibc's vfork
                // keeps the return address in rdi across the syscall).
                let parent = self.task(tid)?;
                if let (Some(e), FORK | VFORK) = (parent.entry_regs, parent.syscall_nr) {
                    if let Ok(c) = self.task_mut(child) {
                        if c.is_stopped() {
                            let mut r = c.regs()?;
                            r.rdi = e.rdi;
                            r.rsi = e.rsi;
                            r.rdx = e.rdx;
                            r.r10 = e.r10;
                            r.r8 = e.r8;
                            r.r9 = e.r9;
                            c.set_regs(&r)?;
                        }
                    }
                }
                if self.task(child)?.tgid == child {
                    // A new process: it inherits per-process bookkeeping and
                    // is a child of the forking process in the emulated tree.
                    let parent_key = self.proc_key(tid)?;
                    self.random_fork(parent_key, ltid);
                    let logical_parent = if sibling {
                        self.parent_of.get(&parent_key).copied().unwrap_or(0)
                    } else {
                        parent_key
                    };
                    self.parent_of.insert(ltid, logical_parent);
                }
                Ok(None)
            }
            Stop::ExecEvent => {
                let t = self.task_mut(tid)?;
                t.in_syscall = true;
                t.syscall_nr = EXECVE;
                self.post_exec(tid)?;
                Ok(None)
            }
            Stop::ExitEvent => Ok(Some(Event::Exit)),
        }
    }

    /// Replay: queue ffs's preemption signal for `ltid`, stopped at a
    /// syscall entry, so the kernel sees a pending signal during the
    /// syscall exactly as in the recording.
    pub fn prearm_preempt(&mut self, ltid: Ltid) -> Result<()> {
        let tid = self.tid_of(ltid)?;
        let tgid = self.task(tid)?.tgid;
        ptrace::tgkill(tgid, tid, TIME_SLICE_SIGNAL)?;
        self.prearmed.insert(tid);
        Ok(())
    }

    /// The port for the next `bind` to port 0: the recorded one in a
    /// replay, otherwise the next of the range.
    fn choose_port(&mut self) -> u16 {
        let i = self.ports_used.len();
        let p = match &self.port_plan {
            Some(plan) if i < plan.len() => plan[i],
            _ => self.fresh_port(),
        };
        self.ports_used.push(p);
        p
    }

    fn fresh_port(&mut self) -> u16 {
        let p = self.next_port;
        self.next_port = if p >= LAST_PORT { FIRST_PORT } else { p + 1 };
        p
    }

    /// Was the last chosen port dictated by a replay's plan?
    fn port_planned(&self) -> bool {
        self.port_plan.as_ref().is_some_and(|plan| self.ports_used.len() <= plan.len())
    }

    /// An ephemeral port the kernel picks for `bind(port 0)` is live state
    /// the program then prints, parses and sends to its peers, so ffs
    /// picks the port instead (deterministically, and recorded in the
    /// trace).  The sockaddr in the tracee is rewritten for the duration
    /// of the syscall; a port the host happens to have in use is retried
    /// with the next one without the program seeing it.
    fn net_pre_syscall(&mut self, tid: pid_t, nr: u64) -> Result<()> {
        if nr != BIND {
            return Ok(());
        }
        let t = self.task_mut(tid)?;
        if t.mem_restore.is_some() {
            return Ok(()); // a retry re-entering with our port in place
        }
        let r = t.regs()?;
        let (addr, len) = (r.rsi, r.rdx);
        if len < 4 {
            return Ok(());
        }
        let mut hdr = [0u8; 4];
        if t.mem.read(addr, &mut hdr).is_err() {
            return Ok(());
        }
        let family = u16::from_ne_bytes([hdr[0], hdr[1]]);
        if (family != AF_INET && family != AF_INET6) || hdr[2] != 0 || hdr[3] != 0 {
            return Ok(());
        }
        let p = self.choose_port();
        let t = self.task_mut(tid)?;
        t.mem.write(addr + 2, &p.to_be_bytes())?;
        t.mem_restore = Some((addr + 2, vec![0, 0]));
        t.bound_port = Some(p);
        t.port_tries = 0;
        Ok(())
    }

    fn pre_syscall(&mut self, _tid: pid_t, nr: u64) -> Result<()> {
        // These interfaces permit writes outside traced syscall boundaries.
        // Refuse setup/submission before they can create asynchronous state.
        if crate::effects::refused_syscall(nr) {
            self.emulate_syscall(_tid, Emulation::Ret(-(libc::ENOSYS as i64)))?;
            return Ok(());
        }
        const CLONE: u64 = 56;
        const FORK: u64 = 57;
        const VFORK: u64 = 58;
        const CLONE3: u64 = 435;
        if matches!(nr, CLONE | FORK | VFORK | CLONE3) {
            // The thread/process about to be created gets the canonical id
            // of the next logical thread.
            if let Some(ns) = &self.opts.ns {
                ns.steer_canonical(self.next_ltid)?;
            }
        }
        self.file_pre_syscall(_tid, nr)?;
        self.proc_pre_syscall(_tid, nr)?;
        self.sigmask_pre_syscall(_tid, nr)?;
        self.net_pre_syscall(_tid, nr)?;
        if self.opts.virtual_time {
            self.cpu_file_pre_syscall(_tid, nr)?;
        }
        if self.task(_tid)?.emulated.is_some() {
            return Ok(());
        }
        if self.opts.virtual_time {
            if nr == PRCTL {
                let t = self.task_mut(_tid)?;
                let r = t.regs()?;
                if r.rdi == libc::PR_SET_TSC as u64 && r.rsi == libc::PR_TSC_ENABLE as u64 {
                    let mut n = r;
                    n.rsi = libc::PR_TSC_SIGSEGV as u64;
                    t.set_regs(&n)?;
                    t.entry_regs = Some(r);
                }
            }
            if nr == RSEQ {
                // The kernel keeps the current cpu number in the rseq area,
                // which would leak real scheduling into the program (e.g.
                // glibc's sched_getcpu).  Pretend the kernel has no rseq.
                self.emulate_syscall(_tid, Emulation::Ret(-(libc::ENOSYS as i64)))?;
                return Ok(());
            }
            self.vt_pre_syscall(_tid, nr)?;
        } else {
            self.task_mut(_tid)?.rewound = false;
        }
        Ok(())
    }

    /// The program must not block the preemption signal: clear it from the
    /// mask a syscall is about to install (rr does the same for its
    /// signal).  The original bytes go back after the syscall.
    fn sigmask_pre_syscall(&mut self, tid: pid_t, nr: u64) -> Result<()> {
        const RT_SIGPROCMASK: u64 = 14;
        const RT_SIGSUSPEND: u64 = 130;
        const PSELECT6: u64 = 270;
        const PPOLL: u64 = 271;
        const EPOLL_PWAIT: u64 = 281;
        const EPOLL_PWAIT2: u64 = 441;
        const SIG_UNBLOCK: u64 = 1;
        let t = self.task_mut(tid)?;
        let r = t.regs()?;
        let ptr = match nr {
            RT_SIGPROCMASK if r.rdi != SIG_UNBLOCK => r.rsi,
            RT_SIGSUSPEND => r.rdi,
            PPOLL => r.r10,
            EPOLL_PWAIT | EPOLL_PWAIT2 => r.r8,
            PSELECT6 if r.r9 != 0 => t.mem.read_u64(r.r9).unwrap_or(0),
            _ => return Ok(()),
        };
        if ptr == 0 || t.mem_restore.is_some() {
            return Ok(());
        }
        let Ok(mask) = t.mem.read_u64(ptr) else { return Ok(()) };
        let bit = 1u64 << (TIME_SLICE_SIGNAL - 1);
        if mask & bit != 0 {
            t.mem.write_u64(ptr, mask & !bit)?;
            // Put the program's bytes back afterwards, unless the kernel
            // writes the old mask into the same buffer.
            if !(nr == RT_SIGPROCMASK && r.rdx == ptr) {
                t.mem_restore = Some((ptr, mask.to_ne_bytes().to_vec()));
            }
        }
        Ok(())
    }

    /// The program sees one cpu: `getcpu` and `sched_getaffinity` say so,
    /// and `/sys/devices/system/cpu/online` (what glibc's `sysconf` reads)
    /// is redirected to a file of ours that says `0`.
    fn cpu_file_pre_syscall(&mut self, tid: pid_t, nr: u64) -> Result<()> {
        const OPEN: u64 = 2;
        const OPENAT: u64 = 257;
        let t = self.task_mut(tid)?;
        let r = t.regs()?;
        let ptr = match nr {
            OPEN => r.rdi,
            OPENAT => r.rsi,
            _ => return Ok(()),
        };
        let Ok(path) = t.mem.read_cstr(ptr, 64) else { return Ok(()) };
        let Some(content) = canonical_file(&path) else { return Ok(()) };
        if self.cpu_online.is_none() {
            self.cpu_online = Some(Rc::new(CpuOnline::new()?));
        }
        let new = format!("{}\0", self.cpu_online.as_ref().unwrap().path_for(content)?);
        let t = self.task_mut(tid)?;
        let mut old = vec![0u8; new.len()];
        t.mem.read(ptr, &mut old)?;
        t.mem.write(ptr, new.as_bytes())?;
        t.mem_restore = Some((ptr, old));
        Ok(())
    }

    fn magic_event(&mut self, tid: pid_t, call: MagicCall) -> Result<Event> {
        let t = self.task_mut(tid)?;
        let r = t.regs()?;
        Ok(match call {
            MagicCall::Checkpoint => {
                let name = t.mem.read_cstr(r.rdi, 256).unwrap_or_default();
                Event::Checkpoint { name }
            }
            MagicCall::Assert => {
                if r.rdi == 0 {
                    let msg = t.mem.read_cstr(r.rsi, 1024).unwrap_or_default();
                    Event::AssertFailed { msg }
                } else {
                    // Passing assertions are not interesting to the scheduler.
                    Event::SyscallEntry {
                        nr: ffs_api::FFS_SYS_ASSERT,
                    }
                }
            }
            MagicCall::Yield => Event::Yield,
            MagicCall::Log => {
                let msg = t.mem.read_cstr(r.rdi, 4096).unwrap_or_default();
                let ltid = t.ltid;
                eprintln!("[ffs] T{ltid}: {msg}");
                self.logs.push(format!("T{ltid}: {msg}"));
                Event::SyscallEntry {
                    nr: ffs_api::FFS_SYS_LOG,
                }
            }
            MagicCall::Active => Event::SyscallEntry {
                nr: ffs_api::FFS_SYS_ACTIVE,
            },
            MagicCall::Begin => Event::Begin,
            MagicCall::End => Event::End,
            MagicCall::Watch => {
                // Every 8-byte slot of [addr, addr+len), at most the 4 debug
                // registers in total.
                let (addr, len) = (r.rdi, r.rsi.max(1));
                let ltid = t.ltid;
                let mut a = addr & !7;
                while a < addr + len && self.watch_requests.len() < 4 {
                    if !self.watch_requests.contains(&a) {
                        self.watch_requests.push(a);
                    }
                    a += 8;
                }
                log::info!("T{ltid}: ffs_watch({addr:#x}, {len})");
                Event::SyscallEntry {
                    nr: ffs_api::FFS_SYS_WATCH,
                }
            }
        })
    }

    /// Overwrite the result of time-related syscalls with virtual time.
    fn virtualize_time_result(&mut self, tid: pid_t, nr: u64) -> Result<()> {
        if matches!(nr, 283 | 3) {
            self.vt_timerfd_exit(tid, nr)?;
        }
        let now = self.virtual_ns();
        let base_rt = self.vclock.base_realtime_ns;
        let base_mono = self.vclock.base_monotonic_ns;
        let t = self.task_mut(tid)?;
        let r = t.regs()?;
        if (r.rax as i64) < 0 {
            return Ok(());
        }
        match nr {
            PRCTL if r.rdi == libc::PR_GET_TSC as u64 => {
                t.mem.write(r.rsi, &t.tsc_mode.to_ne_bytes())?;
            }
            PRCTL if r.rdi == libc::PR_SET_TSC as u64 => {
                t.tsc_mode = r.rsi as i32;
            }
            CLOCK_GETTIME => {
                let ns = match r.rdi {
                    CLOCK_REALTIME => base_rt + now,
                    CLOCK_MONOTONIC => base_mono + now,
                    _ => base_mono + now,
                };
                if r.rsi != 0 {
                    t.mem.write_u64(r.rsi, ns / 1_000_000_000)?;
                    t.mem.write_u64(r.rsi + 8, ns % 1_000_000_000)?;
                }
            }
            GETTIMEOFDAY => {
                let ns = base_rt + now;
                if r.rdi != 0 {
                    t.mem.write_u64(r.rdi, ns / 1_000_000_000)?;
                    t.mem.write_u64(r.rdi + 8, (ns % 1_000_000_000) / 1000)?;
                }
            }
            GETCPU => {
                if r.rdi != 0 {
                    t.mem.write(r.rdi, &0u32.to_ne_bytes())?;
                }
                if r.rsi != 0 {
                    t.mem.write(r.rsi, &0u32.to_ne_bytes())?;
                }
            }
            ADJTIMEX | CLOCK_ADJTIME => {
                // struct timex.time (offset 72) is the current time.
                let buf = if nr == ADJTIMEX { r.rdi } else { r.rsi };
                let ns = base_rt + now;
                if buf != 0 {
                    t.mem.write_u64(buf + 72, ns / 1_000_000_000)?;
                    t.mem.write_u64(buf + 80, (ns % 1_000_000_000) / 1000)?;
                }
            }
            TIMES => {
                // CPU times in clock ticks (CLK_TCK = 100): all of the
                // virtual time counts as user time of this process.
                let ticks = now / 10_000_000;
                if r.rdi != 0 {
                    t.mem.write_u64(r.rdi, ticks)?;
                    t.mem.write_u64(r.rdi + 8, 0)?;
                    t.mem.write_u64(r.rdi + 16, 0)?;
                    t.mem.write_u64(r.rdi + 24, 0)?;
                }
                let mut nr_ = r;
                nr_.rax = (base_mono + now) / 10_000_000;
                t.set_regs(&nr_)?;
            }
            GETRUSAGE => {
                // ru_utime / ru_stime (timevals at offsets 0 and 16).
                if r.rsi != 0 {
                    t.mem.write_u64(r.rsi, now / 1_000_000_000)?;
                    t.mem.write_u64(r.rsi + 8, (now % 1_000_000_000) / 1000)?;
                    t.mem.write_u64(r.rsi + 16, 0)?;
                    t.mem.write_u64(r.rsi + 24, 0)?;
                }
            }
            SCHED_GETAFFINITY => {
                // One virtual cpu (cpu 0), like getcpu; sysconf(_SC_NPROCESSORS_ONLN)
                // in glibc counts this mask.
                let len = (r.rax as usize).min(r.rsi as usize);
                if r.rdx != 0 && len > 0 {
                    let mut mask = vec![0u8; len];
                    mask[0] = 1;
                    t.mem.write(r.rdx, &mask)?;
                }
            }
            TIME => {
                let secs = (base_rt + now) / 1_000_000_000;
                let mut nr_ = r;
                nr_.rax = secs;
                t.set_regs(&nr_)?;
                if r.rdi != 0 {
                    t.mem.write_u64(r.rdi, secs)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// If the SIGSEGV at the current rip is a trapped rdtsc/rdtscp (PR_SET_TSC),
    /// emulate it with virtual time and return true.
    fn try_emulate_rdtsc(&mut self, tid: pid_t) -> Result<bool> {
        let now = self.virtual_ns().wrapping_mul(3); // ~3 GHz TSC
        let t = self.task_mut(tid)?;
        if t.tsc_mode == libc::PR_TSC_SIGSEGV {
            return Ok(false);
        }
        let mut r = t.regs()?;
        let mut insn = [0u8; 3];
        if t.mem.read(r.rip, &mut insn).is_err() {
            return Ok(false);
        }
        let len = if insn[..2] == RDTSC_INSN {
            2
        } else if insn == RDTSCP_INSN {
            r.rcx = 0;
            3
        } else {
            return Ok(false);
        };
        r.rax = now & 0xffff_ffff;
        r.rdx = now >> 32;
        r.rip += len;
        t.set_regs(&r)?;
        t.pending_sig = 0;
        Ok(true)
    }

    /// Replay helper: the task is at a syscall-entry stop; resume it into the
    /// syscall and mark it blocked without waiting for it (the trace says it
    /// blocked here).
    pub fn resume_as_blocked(&mut self, ltid: Ltid) -> Result<Event> {
        let tid = self.tid_of(ltid)?;
        if let Some(stop @ Stop::SyscallExit { ret, .. }) = self.task(tid)?.stop() {
            if ret == -(libc::EINTR as i64) {
                if let Some(event) = self.handle_stop(tid, stop)? {
                    return Ok(event);
                }
            }
        }
        let stop = self.task(tid)?.stop();
        if let Some(Stop::SyscallExit { ret, .. }) = stop {
            if ptrace::is_restart_result(ret as u64) || self.task(tid)?.restarting {
                // The kernel restarts the interrupted syscall on resume (its
                // entry stop is invisible); the trace says it then blocks.
                self.task_mut(tid)?.restarting = true;
                // Drain the internal signal and restarted entry before
                // claiming the task is back in the kernel. Otherwise the
                // virtual clock could expire its wait at a signal stop.
                return self.run_task(ltid, 0);
            }
        }
        let Some(Stop::SyscallEntry { nr }) = stop else {
            bail!(
                "task T{ltid} is not at a syscall entry (state {:?})",
                self.task(tid)?.state
            );
        };
        if matches!(nr, WAIT4 | WAITID) || (self.opts.virtual_time && matches!(nr, NANOSLEEP | CLOCK_NANOSLEEP)) {
            // Emulated: the block is decided at the (immediate) exit stop.
            return self.run_task(ltid, 0);
        }
        self.vclock.events += 1;
        self.pre_syscall(tid, nr)?;
        let t = self.task_mut(tid)?;
        t.resume(ResumeHow::Syscall, 0)?;
        const CLONE: u64 = 56;
        const CLONE3: u64 = 435;
        if matches!(nr, CLONE | FORK | VFORK | CLONE3) {
            // A vfork parent blocks after the clone event, which registers
            // the child; take that event before considering it blocked.
            match self.wait_task(tid, true)? {
                WaitOutcome::Exited => {
                    let code = self.task(tid)?.exit_code.unwrap_or(0);
                    return Ok(Event::Exited { code });
                }
                WaitOutcome::Blocked => return Ok(Event::Blocked { nr }),
                WaitOutcome::Stopped(stop @ Stop::CloneEvent { .. }) => {
                    self.handle_stop(tid, stop)?;
                    self.task_mut(tid)?.resume(ResumeHow::Syscall, 0)?;
                }
                WaitOutcome::Stopped(stop) => {
                    if let Some(ev) = self.handle_stop(tid, stop)? {
                        return Ok(ev);
                    }
                    self.task_mut(tid)?.resume(ResumeHow::Syscall, 0)?;
                }
            }
        }
        // The trace says the syscall blocks; make sure the thread really is
        // asleep in the kernel before the schedule moves on.  Otherwise the
        // next thread's wakeup (a futex wake recorded as waking one waiter)
        // can run before this one has queued itself and wake nobody.
        let start = Instant::now();
        loop {
            if let Some(st) = ptrace::try_wait_tid(tid)? {
                // It completed (or died) at once; the recording saw that
                // later, as an asynchronous stop.
                let t = self.task_mut(tid)?;
                if t.did_wait(st)?.is_some() {
                    t.unreported = true;
                }
                return Ok(Event::Blocked { nr });
            }
            match ptrace::proc_state(tid) {
                Some('S') | Some('D') => break,
                Some('Z') | Some('X') | None => break,
                _ if start.elapsed() > self.opts.deadlock_timeout => break,
                _ => std::thread::sleep(Duration::from_micros(10)),
            }
        }
        self.task_mut(tid)?.state = State::Blocked { nr };
        Ok(Event::Blocked { nr })
    }

    /// Replay helper: run the task until its cumulative ticks reach `ticks`
    /// with the instruction pointer at `rip` (the point where an asynchronous
    /// event such as a preemption was recorded), or until an earlier event
    /// occurs.  Returns `Event::Preempt` on reaching the target, unless a
    /// signal `(sig, siginfo)` is expected there: then the signal is made to
    /// arrive at that point (rr's asynchronous signal replay) and the
    /// resulting event is returned.
    pub fn run_task_to(
        &mut self,
        ltid: Ltid,
        ticks: u64,
        rip: u64,
        signal: Option<&(i32, String)>,
    ) -> Result<Event> {
        let tid = self.tid_of(ltid)?;
        let skid = self.pmu.skid;
        {
            let t = self.task(tid)?;
            if matches!(t.state, State::Blocked { .. }) && signal.is_some() {
                // The signal interrupted a blocking syscall: interrupt it the
                // same way and deliver the signal at the interruption point.
                self.vclock.events += 1;
                self.interrupt_blocked(tid)?;
                let t = self.task(tid)?;
                if t.ticks == ticks && (!t.is_stopped() || t.unreported) {
                    return self.run_task(ltid, 0);
                }
            } else if t.unreported || !t.is_stopped() {
                return self.run_task(ltid, 0);
            } else {
                self.vclock.events += 1;
            }
        }
        loop {
            let t = self.task_mut(tid)?;
            let cur = t.ticks;
            if cur > ticks {
                // The execution has already diverged from the trace (the
                // thread ran past the recorded point); report where it is.
                log::debug!("overshot tick target: T{ltid} is at {cur} ticks, wanted {ticks}");
                return Ok(Event::Preempt);
            }
            let cur_rip = t.rip();
            if cur == ticks && cur_rip == rip && self.prearmed.remove(&tid) && signal.is_none() {
                // The preemption signal queued before the syscall is
                // delivered now that it returned: that stop is the
                // recorded preemption.
                let t = self.task_mut(tid)?;
                t.resume(ResumeHow::Syscall, 0)?;
                return Ok(match self.wait_task(tid, false)? {
                    WaitOutcome::Exited => {
                        let code = self.task(tid)?.exit_code.unwrap_or(0);
                        Event::Exited { code }
                    }
                    WaitOutcome::Blocked => {
                        let nr = self.task(tid)?.syscall_nr;
                        Event::Blocked { nr }
                    }
                    WaitOutcome::Stopped(stop) => self.handle_stop(tid, stop)?.unwrap_or(Event::Preempt),
                });
            }
            let t = self.task_mut(tid)?;
            if cur == ticks && cur_rip == rip {
                // Make the state canonical.
                if let State::Stopped(Stop::Signal { sig }) = t.state {
                    if sig == TIME_SLICE_SIGNAL || sig == libc::SIGTRAP {
                        t.pending_sig = 0;
                        t.state = State::Stopped(Stop::Parked);
                    }
                }
                if let Some((sig, info)) = signal {
                    return self.deliver_signal(tid, *sig, info);
                }
                return Ok(Event::Preempt);
            }
            let remaining = ticks - cur;
            // Approach the target with the counter, `skid` ticks early (the
            // interrupt can land that late under load; a smaller second
            // stage overshot in practice).  The last window is covered by
            // a hardware breakpoint on the target instruction, one ptrace
            // round trip per hit.
            let margin = skid;
            if remaining > margin {
                let stop = t.stop().unwrap();
                let (how, allow_block, budget) = match stop {
                    Stop::SyscallEntry { nr } => {
                        self.pre_syscall(tid, nr)?;
                        (ResumeHow::Syscall, true, 0)
                    }
                    Stop::CloneEvent { .. } => (ResumeHow::Syscall, true, 0),
                    Stop::ExitEvent => (ResumeHow::Cont, false, 0),
                    _ => (ResumeHow::Syscall, false, remaining - margin),
                };
                self.task_mut(tid)?.resume(how, budget)?;
                match self.wait_task(tid, allow_block)? {
                    WaitOutcome::Exited => {
                        let code = self.task(tid)?.exit_code.unwrap_or(0);
                        return Ok(Event::Exited { code });
                    }
                    WaitOutcome::Blocked => {
                        let nr = self.task(tid)?.syscall_nr;
                        return Ok(Event::Blocked { nr });
                    }
                    WaitOutcome::Stopped(stop) => match self.handle_stop(tid, stop)? {
                        None | Some(Event::Preempt) => continue,
                        Some(ev) => return Ok(ev),
                    },
                }
            }
            // Precise approach.  With a hardware execute breakpoint on the
            // target, arriving there early is cheap to get past: the
            // resume flag lets the instruction execute once without
            // re-trapping, so the thread is simply continued.  Otherwise
            // (int3) the thread must be single-stepped off the target
            // before a breakpoint there is useful; the last tick is always
            // single-stepped, and a syscall instruction at the target is
            // run one stop at a time.
            let hw_bp = self.watchpoints.len() < 4;
            let mut insn = [0u8; 2];
            if cur_rip == rip || remaining <= 1 {
                let t = self.task_mut(tid)?;
                // Unreadable (a jump to a bad address about to fault): not
                // a syscall instruction.
                let _ = t.mem.read(cur_rip, &mut insn);
            }
            let at_syscall = (cur_rip == rip || remaining <= 1)
                && (insn == ptrace::SYSCALL_INSN || matches!(self.task(tid)?.stop(), Some(Stop::SyscallEntry { .. })));
            let must_step = remaining <= 1 || (cur_rip == rip && !hw_bp);
            if at_syscall || must_step {
                let t = self.task_mut(tid)?;
                if at_syscall {
                    // The next thing is a syscall.  Run it one stop at a
                    // time: a visible event is returned for the caller to
                    // compare against the trace; an invisible one (the exit
                    // of a magic call) just moves the thread on.
                    let stop = t.stop().unwrap();
                    let mut allow_block = false;
                    if let Stop::SyscallEntry { nr } = stop {
                        allow_block = true;
                        self.pre_syscall(tid, nr)?;
                    }
                    let how = if stop == Stop::ExitEvent { ResumeHow::Cont } else { ResumeHow::Syscall };
                    self.task_mut(tid)?.resume(how, 0)?;
                    match self.wait_task(tid, allow_block)? {
                        WaitOutcome::Exited => {
                            let code = self.task(tid)?.exit_code.unwrap_or(0);
                            return Ok(Event::Exited { code });
                        }
                        WaitOutcome::Blocked => {
                            let nr = self.task(tid)?.syscall_nr;
                            return Ok(Event::Blocked { nr });
                        }
                        WaitOutcome::Stopped(stop) => match self.handle_stop(tid, stop)? {
                            Some(ev) => return Ok(ev),
                            None => continue,
                        },
                    }
                }
                t.resume(ResumeHow::SingleStep, 0)?;
                match self.wait_task(tid, false)? {
                    WaitOutcome::Exited => {
                        let code = self.task(tid)?.exit_code.unwrap_or(0);
                        return Ok(Event::Exited { code });
                    }
                    WaitOutcome::Blocked => {
                            // Not expected to block here (a replay that no
                            // longer follows its trace): report it.
                            let nr = self.task(tid)?.syscall_nr;
                            return Ok(Event::Blocked { nr });
                        }
                    WaitOutcome::Stopped(Stop::Signal { sig }) if sig == libc::SIGTRAP => {
                        let t = self.task_mut(tid)?;
                        t.state = State::Stopped(Stop::Parked);
                        continue;
                    }
                    WaitOutcome::Stopped(stop) => match self.handle_stop(tid, stop)? {
                        None | Some(Event::Preempt) => continue,
                        Some(ev) => return Ok(ev),
                    },
                }
            } else if hw_bp {
                // Hardware execute breakpoint on the target (stays armed
                // across hits; the next ordinary resume disarms it).
                let watch = self.watchpoints.clone();
                let t = self.task_mut(tid)?;
                if t.exec_bp != Some(rip) {
                    crate::watch::arm_exec_breakpoint(tid, rip, &watch)?;
                    t.exec_bp = Some(rip);
                    t.watch_addrs = watch;
                }
                if cur_rip == rip {
                    // At the target too early: make sure the resume flag
                    // is set (the kernel sets it after a breakpoint trap,
                    // not after a step or a counter interrupt) so the
                    // instruction executes instead of trapping again.
                    let mut r = t.regs()?;
                    if r.eflags & X86_RF == 0 {
                        r.eflags |= X86_RF;
                        t.set_regs(&r)?;
                    }
                }
                let stop = t.stop().unwrap();
                let allow_block = matches!(stop, Stop::SyscallEntry { .. });
                if let Stop::SyscallEntry { nr } = stop {
                    self.pre_syscall(tid, nr)?;
                }
                let t = self.task_mut(tid)?;
                let how = if stop == Stop::ExitEvent { ResumeHow::Cont } else { ResumeHow::Syscall };
                t.keep_exec_bp = true;
                let r = t.resume(how, 0);
                t.keep_exec_bp = false;
                r?;
                match self.wait_task(tid, allow_block)? {
                    WaitOutcome::Exited => {
                        let code = self.task(tid)?.exit_code.unwrap_or(0);
                        return Ok(Event::Exited { code });
                    }
                    WaitOutcome::Blocked => {
                        let nr = self.task(tid)?.syscall_nr;
                        return Ok(Event::Blocked { nr });
                    }
                    WaitOutcome::Stopped(Stop::Signal { sig }) if sig == libc::SIGTRAP => {
                        // Our breakpoint, or a watchpoint of the
                        // exploration (then DR6 says which; with none set
                        // the trap at the target is ours).
                        let hit = if self.watchpoints.is_empty() {
                            self.task_mut(tid)?.rip() == rip
                        } else {
                            crate::watch::watchpoint_hits(tid) & (1 << crate::watch::EXEC_SLOT) != 0
                        };
                        let t = self.task_mut(tid)?;
                        if hit {
                            t.state = State::Stopped(Stop::Parked);
                            continue;
                        }
                        // Some other trap (e.g. the tracee's own int3).
                        t.pending_sig = sig;
                        let info = ptrace::get_siginfo(tid).map(|i| ptrace::siginfo_to_hex(&i)).unwrap_or_default();
                        return Ok(Event::Signal { sig, info });
                    }
                    WaitOutcome::Stopped(stop) => match self.handle_stop(tid, stop)? {
                        None | Some(Event::Preempt) => continue,
                        Some(ev) => return Ok(ev),
                    },
                }
            } else {
                let t = self.task_mut(tid)?;
                let mut saved = [0u8; 1];
                if t.mem.read(rip, &mut saved).is_err() || t.mem.write(rip, &[0xcc]).is_err() {
                    // The target is not mapped (yet): code the program is
                    // about to map or make executable.  Single-step instead.
                    t.resume(ResumeHow::SingleStep, 0)?;
                    match self.wait_task(tid, false)? {
                        WaitOutcome::Exited => {
                            let code = self.task(tid)?.exit_code.unwrap_or(0);
                            return Ok(Event::Exited { code });
                        }
                        WaitOutcome::Blocked => {
                            // Not expected to block here (a replay that no
                            // longer follows its trace): report it.
                            let nr = self.task(tid)?.syscall_nr;
                            return Ok(Event::Blocked { nr });
                        }
                        WaitOutcome::Stopped(Stop::Signal { sig }) if sig == libc::SIGTRAP => {
                            self.task_mut(tid)?.state = State::Stopped(Stop::Parked);
                            continue;
                        }
                        WaitOutcome::Stopped(stop) => match self.handle_stop(tid, stop)? {
                            None | Some(Event::Preempt) => continue,
                            Some(ev) => return Ok(ev),
                        },
                    }
                }
                let stop = t.stop().unwrap();
                let allow_block = matches!(stop, Stop::SyscallEntry { .. });
                if let Stop::SyscallEntry { nr } = stop {
                    self.pre_syscall(tid, nr)?;
                }
                let t = self.task_mut(tid)?;
                let how = if stop == Stop::ExitEvent {
                    ResumeHow::Cont
                } else {
                    ResumeHow::Syscall
                };
                t.resume(how, 0)?;
                let outcome = self.wait_task(tid, allow_block)?;
                // Remove the breakpoint whatever happened.
                if let Ok(t) = self.task_mut(tid) {
                    if t.is_alive() {
                        let _ = t.mem.write(rip, &saved);
                    }
                }
                match outcome {
                    WaitOutcome::Exited => {
                        let code = self.task(tid)?.exit_code.unwrap_or(0);
                        return Ok(Event::Exited { code });
                    }
                    WaitOutcome::Blocked => {
                        let nr = self.task(tid)?.syscall_nr;
                        return Ok(Event::Blocked { nr });
                    }
                    WaitOutcome::Stopped(Stop::Signal { sig }) if sig == libc::SIGTRAP => {
                        let t = self.task_mut(tid)?;
                        let mut r = t.regs()?;
                        if r.rip == rip + 1 {
                            r.rip = rip;
                            t.set_regs(&r)?;
                            t.state = State::Stopped(Stop::Parked);
                            continue;
                        }
                        // Some other trap (e.g. the tracee's own int3).
                        t.pending_sig = sig;
                        let info = ptrace::get_siginfo(tid).map(|i| ptrace::siginfo_to_hex(&i)).unwrap_or_default();
                        return Ok(Event::Signal { sig, info });
                    }
                    WaitOutcome::Stopped(stop) => match self.handle_stop(tid, stop)? {
                        None | Some(Event::Preempt) => continue,
                        Some(ev) => return Ok(ev),
                    },
                }
            }
        }
    }

    /// Single-step the (stopped) task `n` instructions, stopping early at a
    /// `syscall` instruction or any other event.  Returns `Preempt` if the
    /// steps completed.
    pub fn step_instructions(&mut self, ltid: Ltid, n: u32) -> Result<Event> {
        let tid = self.tid_of(ltid)?;
        for _ in 0..n {
            let t = self.task_mut(tid)?;
            if !t.is_stopped() || t.pending_sig != 0 {
                break;
            }
            let rip = t.rip();
            let mut insn = [0u8; 2];
            if t.mem.read(rip, &mut insn).is_err() || insn == ptrace::SYSCALL_INSN {
                break;
            }
            t.resume(ResumeHow::SingleStep, 0)?;
            match self.wait_task(tid, false)? {
                WaitOutcome::Exited => {
                    let code = self.task(tid)?.exit_code.unwrap_or(0);
                    return Ok(Event::Exited { code });
                }
                WaitOutcome::Blocked => {
                            // Not expected to block here (a replay that no
                            // longer follows its trace): report it.
                            let nr = self.task(tid)?.syscall_nr;
                            return Ok(Event::Blocked { nr });
                        }
                WaitOutcome::Stopped(Stop::Signal { sig }) if sig == libc::SIGTRAP => {
                    self.task_mut(tid)?.state = State::Stopped(Stop::Parked);
                    if !self.watchpoints.is_empty() {
                        let bits = crate::watch::watchpoint_hits(tid);
                        if bits != 0 {
                            // The stepped instruction touched a watched
                            // variable: that is the scheduling point.
                            self.watch_account(bits)?;
                            self.watch_preempt = true;
                            break;
                        }
                    }
                }
                WaitOutcome::Stopped(stop) => {
                    if let Some(ev) = self.handle_stop(tid, stop)? {
                        return Ok(ev);
                    }
                }
            }
        }
        if !self.watchpoints.is_empty() {
            // After single-stepping, the next hit of a data breakpoint is
            // not reported unless the registers are programmed again.
            self.watch_new_task(tid);
        }
        Ok(Event::Preempt)
    }

    /// Interrupt a task blocked in a syscall and leave it at the stop where
    /// the kernel has the syscall interrupted (result `-ERESTART*`, restart
    /// pending), i.e. exactly the state in which an asynchronous signal
    /// would find it.  If the syscall completed instead, the completion is
    /// left unreported.
    pub fn interrupt_blocked(&mut self, tid: pid_t) -> Result<()> {
        ptrace::interrupt(tid)?;
        let t = self.task_mut(tid)?;
        t.interrupt_pending = true;
        let st = ptrace::wait_tid(tid)?.ok_or_else(|| anyhow!("task {tid} vanished"))?;
        let t = self.task_mut(tid)?;
        match t.did_wait(st)? {
            None => {}
            Some(Stop::SyscallExit { ret, .. }) if ptrace::is_restart_result(ret as u64) => {
                t.restarting = true;
                t.state = State::Stopped(Stop::Parked);
            }
            Some(Stop::Parked) => {
                let r = t.regs()?;
                if (r.orig_rax as i64) >= 0 && ptrace::is_restart_result(r.rax) {
                    t.in_syscall = false;
                    t.restarting = true;
                }
            }
            Some(_) => {
                t.unreported = true;
            }
        }
        Ok(())
    }

    /// Make signal `sig` (with the recorded `siginfo`, hex) arrive at the
    /// current point of the stopped task `tid` and return the event.  A
    /// signal already pending in the kernel is simply let through; a
    /// synchronous fault is reproduced by executing the faulting
    /// instruction; anything else is sent with `tgkill` and its siginfo
    /// replaced by the recorded one.
    fn deliver_signal(&mut self, tid: pid_t, sig: i32, info_hex: &str) -> Result<Event> {
        let info = ptrace::siginfo_from_hex(info_hex);
        let synchronous = matches!(
            sig,
            libc::SIGSEGV | libc::SIGBUS | libc::SIGILL | libc::SIGFPE | libc::SIGTRAP
        ) && info.map(|i| ptrace::siginfo_code(&i) > 0).unwrap_or(false);
        let pending = ptrace::pending_signals(tid) & (1u64 << (sig - 1)) != 0;
        let mut injected = false;
        if !pending && !synchronous {
            let tgid = self.task(tid)?.tgid;
            ptrace::tgkill(tgid, tid, sig)?;
            injected = true;
        }
        self.expected_signal = Some(sig);
        let r = self.deliver_signal_loop(tid, sig, synchronous, injected, info.as_ref());
        self.expected_signal = None;
        r
    }

    fn deliver_signal_loop(
        &mut self,
        tid: pid_t,
        sig: i32,
        synchronous: bool,
        injected: bool,
        info: Option<&[u8; 128]>,
    ) -> Result<Event> {
        loop {
            let stop = self.task(tid)?.stop();
            let mut allow_block = false;
            if let Some(Stop::SyscallEntry { nr }) = stop {
                // The signal is delivered by the syscall the thread is
                // entering (it unblocks the signal, or is interrupted by
                // it): enter it the normal way.
                self.pre_syscall(tid, nr)?;
                allow_block = true;
            }
            let t = self.task_mut(tid)?;
            let how = if synchronous {
                ResumeHow::SingleStep
            } else if stop == Some(Stop::ExitEvent) {
                ResumeHow::Cont
            } else {
                ResumeHow::Syscall
            };
            t.resume(how, 0)?;
            match self.wait_task(tid, allow_block)? {
                WaitOutcome::Exited if sig == libc::SIGKILL => {
                    // A SIGKILL from outside skips the exit stop; the
                    // recording saw one, so report that first.
                    self.task_mut(tid)?.exit_reported = true;
                    return Ok(Event::Exit);
                }
                WaitOutcome::Exited => {
                    let code = self.task(tid)?.exit_code.unwrap_or(0);
                    return Ok(Event::Exited { code });
                }
                WaitOutcome::Blocked => {
                    // The syscall did not deliver the signal after all.
                    let nr = self.task(tid)?.syscall_nr;
                    return Ok(Event::Blocked { nr });
                }
                WaitOutcome::Stopped(Stop::Signal { sig: s }) if s == sig => {
                    if let (true, Some(i)) = (injected, info) {
                        ptrace::set_siginfo(tid, i)?;
                    }
                    return Ok(self
                        .handle_stop(tid, Stop::Signal { sig })?
                        .unwrap_or(Event::Preempt));
                }
                WaitOutcome::Stopped(Stop::Parked) => continue, // interrupt trap
                WaitOutcome::Stopped(Stop::Signal { sig: s }) if synchronous && s == libc::SIGTRAP => {
                    // Single-step trap: the instruction did not fault.
                    let t = self.task_mut(tid)?;
                    t.state = State::Stopped(Stop::Parked);
                    return Ok(Event::Preempt);
                }
                WaitOutcome::Stopped(stop) => match self.handle_stop(tid, stop)? {
                    // An internal stop (the syscall interrupted with a
                    // restart result, a magic-call exit): keep going, the
                    // signal stop comes next.
                    None => continue,
                    Some(ev) => return Ok(ev),
                },
            }
        }
    }

    /// Kill every tracee (used on error paths and when discarding a branch)
    /// and reap them, resuming any that stop on the way out (a SIGKILLed
    /// tracee still reports its PTRACE_EVENT_EXIT stop).
    pub fn kill_all(&mut self) {
        let tgids: std::collections::BTreeSet<pid_t> =
            self.tasks.values().filter(|t| t.is_alive()).map(|t| t.tgid).collect();
        for tgid in &tgids {
            unsafe {
                libc::kill(*tgid, libc::SIGKILL);
            }
        }
        // A task may already be held at an exit stop we consumed while
        // finishing the main process. SIGKILL does not release that stop,
        // and waitpid will not report it a second time.
        for t in self.tasks.values_mut().filter(|t| t.is_alive() && t.is_ptrace_stopped()) {
            let _ = t.hpc.stop();
            let _ = ptrace::resume(t.tid, ResumeHow::Cont, 0);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let mut pending = false;
            for t in self.tasks.values_mut().filter(|t| t.is_alive()) {
                let _ = t.hpc.stop();
                match ptrace::try_wait_tid(t.tid) {
                    Ok(Some(WaitStatus::Exited(_))) | Ok(Some(WaitStatus::Signaled(_))) | Ok(None) if false => {}
                    Ok(Some(WaitStatus::Exited(_))) | Ok(Some(WaitStatus::Signaled(_))) => t.state = State::Exited,
                    Ok(Some(_)) => {
                        let _ = ptrace::resume(t.tid, ResumeHow::Cont, 0);
                        pending = true;
                    }
                    Ok(None) => {
                        if ptrace::proc_state(t.tid).is_none() {
                            t.state = State::Exited;
                        } else {
                            pending = true;
                        }
                    }
                    Err(_) => t.state = State::Exited,
                }
            }
            if !pending || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_micros(100));
        }
        self.reap_zombies();
        self.zombies.clear();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.kill_all();
    }
}

/// Keep the redirection short enough for the original pathname buffer,
/// regardless of TMPDIR (Nix shells commonly use a much longer directory).
/// mkstemp creates an exclusively owned file; checkpoints share its lifetime.
/// Files whose kernel-provided contents are live state that a program
/// parses: the tracee gets a canonical copy instead (the path in its
/// `open` is rewritten to a file the tracer created).  The cpu lists make
/// the program see one cpu.  The mount table and cgroup membership change
/// their mount ids and names from run to run (HotSpot parses
/// `/proc/self/mountinfo` at startup to find cgroup limits, and the number
/// of digits changes its branch count), so a canonical table with no
/// cgroup controllers is served; `/proc/stat` and `/proc/meminfo` are
/// counters that change every millisecond.
pub fn canonical_file(path: &str) -> Option<&'static str> {
    Some(match path {
        "/sys/devices/system/cpu/online" | "/sys/devices/system/cpu/possible" | "/sys/devices/system/cpu/present" => "0\n",
        "/proc/self/mountinfo" | "/proc/thread-self/mountinfo" => {
            "1 0 0:1 / / rw - rootfs rootfs rw\n2 1 0:2 / /proc rw,nosuid,nodev,noexec - proc proc rw\n"
        }
        "/proc/self/mounts" | "/proc/mounts" | "/proc/thread-self/mounts" => {
            "rootfs / rootfs rw 0 0\nproc /proc proc rw,nosuid,nodev,noexec 0 0\n"
        }
        "/proc/self/cgroup" | "/proc/thread-self/cgroup" => "0::/\n",
        "/proc/cgroups" => "#subsys_name\thierarchy\tnum_cgroups\tenabled\n",
        "/proc/stat" => {
            "cpu  0 0 0 0 0 0 0 0 0 0\ncpu0 0 0 0 0 0 0 0 0 0 0\nintr 0\nctxt 0\nbtime 1700000000\nprocesses 1\nprocs_running 1\nprocs_blocked 0\nsoftirq 0 0 0 0 0 0 0 0 0 0 0\n"
        }
        "/proc/meminfo" => {
            "MemTotal:       16777216 kB\nMemFree:         8388608 kB\nMemAvailable:   12582912 kB\nBuffers:               0 kB\nCached:          4194304 kB\nSwapCached:            0 kB\nActive:          4194304 kB\nInactive:        2097152 kB\nSwapTotal:             0 kB\nSwapFree:              0 kB\nDirty:                 0 kB\nShmem:                 0 kB\nCommitLimit:     8388608 kB\nCommitted_AS:    4194304 kB\nHugepagesize:       2048 kB\n"
        }
        _ => return None,
    })
}

/// The tracer's canonical files (see `canonical_file`), one per content.
pub struct CpuOnline(std::cell::RefCell<BTreeMap<&'static str, String>>);

impl CpuOnline {
    fn new() -> Result<Self> {
        Ok(Self(std::cell::RefCell::new(BTreeMap::new())))
    }

    /// Path of a file holding `content`, created on first use.
    fn path_for(&self, content: &'static str) -> Result<String> {
        use std::io::Write;
        use std::os::fd::FromRawFd;
        if let Some(p) = self.0.borrow().get(content) {
            return Ok(p.clone());
        }
        let mut path = b"/tmp/ffs-file-XXXXXX\0".to_vec();
        let fd = unsafe { libc::mkostemp(path.as_mut_ptr().cast(), libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        path.pop();
        let p = String::from_utf8(path)?;
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        file.write_all(content.as_bytes())?;
        // Readable by the tracee, which runs as "nobody" in its user namespace.
        let _ = std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o644));
        self.0.borrow_mut().insert(content, p.clone());
        Ok(p)
    }
}

impl Drop for CpuOnline {
    fn drop(&mut self) {
        for p in self.0.borrow().values() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// An asynchronous signal that did not originate inside the session: a
/// timer signal from the kernel, or a signal sent by a process outside the
/// tracees' pid namespace (`si_pid` is 0 there).
fn is_external_async(sig: i32, info: &[u8; 128]) -> bool {
    const SI_KERNEL: i32 = 0x80;
    const SI_TIMER: i32 = -2;
    let code = ptrace::siginfo_code(info);
    let pid = i32::from_ne_bytes(info[16..20].try_into().unwrap());
    if code == SI_KERNEL {
        return matches!(sig, libc::SIGALRM | libc::SIGVTALRM | libc::SIGPROF | libc::SIGIO);
    }
    code == SI_TIMER || (code <= 0 && pid == 0)
}

/// Inode of the socket open on `fd` in `tid`'s process.
/// Set by the SIGTERM handler; the drivers end the run when they see it.
pub static TERMINATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_sigterm(_sig: libc::c_int) {
    TERMINATED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Signal dispositions of the tracer: the program shares our process
/// group, so a `kill(0, sig)` or a terminal signal reaches ffs too.  Like
/// rr, ignore what a program may legitimately broadcast and turn SIGTERM
/// into an orderly end of the run.
pub fn install_tracer_signal_handlers() {
    unsafe {
        for sig in [
            libc::SIGHUP,
            libc::SIGINT,
            libc::SIGABRT,
            libc::SIGQUIT,
            libc::SIGTRAP,
            libc::SIGTTIN,
            libc::SIGTTOU,
            libc::SIGWINCH,
            libc::SIGUSR1,
            libc::SIGUSR2,
            libc::SIGPIPE,
        ] {
            libc::signal(sig, libc::SIG_IGN);
        }
        // SIGCHLD is consumed with sigtimedwait in `wait_task`.
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGCHLD);
        libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_sigterm as extern "C" fn(libc::c_int) as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
    }
}

pub fn socket_inode(tid: pid_t, fd: i32) -> Option<u64> {
    let t = std::fs::read_link(format!("/proc/{tid}/fd/{fd}")).ok()?;
    let t = t.to_string_lossy();
    t.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
}

pub fn proc_tgid(tid: pid_t) -> Option<pid_t> {
    let s = std::fs::read_to_string(format!("/proc/{tid}/status")).ok()?;
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("Tgid:") {
            return v.trim().parse().ok();
        }
    }
    None
}
