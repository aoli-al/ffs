//! A `Task` is one traced thread (rr's `Task`).  It owns the thread's ticks
//! counter and caches its registers while it is stopped.

use anyhow::{bail, Result};
use libc::pid_t;

use crate::mem::Mem;
use crate::perf::{PmuConfig, TicksCounter};
use crate::ptrace::{self, Regs, ResumeHow, WaitStatus, PTRACE_EVENT_STOP, SYSCALL_INSN_LEN};
use crate::proc::VWait;
use crate::vtime::{Emulation, VBlock};

/// Logical thread id: stable across checkpoint forks, assigned in creation order.
pub type Ltid = u32;

/// The ptrace stop a task is sitting at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Plain stop: next resume executes the instruction at `rip`.  Also the
    /// state after PTRACE_INTERRUPT, the initial attach stop, and the state of
    /// a new thread before its first instruction.
    Parked,
    /// At a syscall-entry stop: the syscall has not been executed yet.
    SyscallEntry { nr: u64 },
    /// At a syscall-exit stop.
    SyscallExit { nr: u64, ret: i64 },
    /// At a signal-delivery stop.  `TIME_SLICE_SIGNAL` means a preemption.
    Signal { sig: i32 },
    /// A `PTRACE_EVENT_CLONE/FORK/VFORK` stop with the child tid.
    CloneEvent { child: pid_t, event: i32 },
    /// A `PTRACE_EVENT_EXEC` stop.
    ExecEvent,
    /// A `PTRACE_EVENT_EXIT` stop: the thread is about to die.
    ExitEvent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Stopped(Stop),
    /// Resumed by us and not yet seen to stop (may be inside a syscall).
    Running,
    /// Observed to be blocked inside a kernel syscall; we switched away.
    Blocked {
        nr: u64,
    },
    /// Reaped.
    Exited,
}

/// What a syscall can block on.  A thread sleeping in a `Tracee` syscall
/// can only be woken by another tracee (all stopped) or a virtual timer, so
/// seeing it asleep means it is blocked, deterministically.  An `External`
/// syscall (sockets whose peer is not a tracee, devices) may complete on
/// its own after a short sleep, so it gets a real-time grace.  `Never`:
/// disk I/O and the like, waited for however long it takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BlockClass {
    #[default]
    Never,
    Tracee,
    External,
}

pub struct Task {
    pub tid: pid_t,
    pub tgid: pid_t,
    pub ltid: Ltid,
    pub state: State,
    pub mem: Mem,
    pub hpc: TicksCounter,
    /// Cumulative user-space ticks executed by this thread.
    pub ticks: u64,
    regs: Option<Regs>,
    /// Signal to deliver on next resume.
    pub pending_sig: i32,
    /// True between a syscall-entry stop and the matching exit stop.
    pub in_syscall: bool,
    /// Syscall number of the syscall in progress (valid when `in_syscall`).
    pub syscall_nr: u64,
    /// What the syscall being executed can block on (decided at its entry).
    pub block_class: BlockClass,
    /// Set when a PTRACE_INTERRUPT was requested and its stop not yet consumed.
    pub interrupt_pending: bool,
    /// Number of exec events seen.
    pub execs: u32,
    pub exit_code: Option<i32>,
    /// The vDSO has been patched in this task's address space.
    pub vdso_patched: bool,
    /// Application-visible PR_SET_TSC mode; the real mode stays SIGSEGV
    /// so ffs can implement virtual rdtsc even when the application enables it.
    pub tsc_mode: i32,
    /// The task reached a stop asynchronously (while blocked) that has not
    /// been reported to the scheduler yet.
    pub unreported: bool,
    /// Logical id of the child created by the syscall in progress.
    pub last_child: Option<Ltid>,
    /// The syscall in progress was interrupted by the preemption signal and
    /// is being transparently restarted by the kernel.
    pub restarting: bool,
    /// Active rseq registration of this thread: (area, length, signature),
    /// recorded from the thread's own `rseq` syscall.  Needed because a
    /// thread recreated by `clone(CLONE_VM)` starts unregistered.
    pub rseq: Option<(u64, u64, u64)>,
    /// The syscall being entered was skipped; its result is supplied at the
    /// exit stop (see `vtime.rs`).
    pub emulated: Option<Emulation>,
    /// Blocked by the tracer rather than the kernel (virtual sleep).
    pub vblock: Option<VBlock>,
    /// Virtual deadline and timeout result of the blocking syscall in
    /// progress, whose real timeout was stripped.
    pub vdeadline: Option<(u64, i64)>,
    /// Argument registers before a timeout was stripped (restored after).
    pub entry_regs: Option<Regs>,
    /// The task was rewound to re-execute a syscall it had already entered.
    pub rewound: bool,
    /// Emulated `wait4`/`waitid` in progress (see `proc.rs`).
    pub vwait: Option<VWait>,
    /// Tracee memory to put back after the current syscall (bytes we
    /// changed in its arguments).
    pub mem_restore: Option<(u64, Vec<u8>)>,
    /// Raw wait status once reaped (`code << 8`, or the signal number).
    pub exit_status: Option<i32>,
    /// An `Exit` event was reported for this task (replay bookkeeping for
    /// threads that die without an exit stop).
    pub exit_reported: bool,
    /// `ptrace::sig_epoch()` when this parked sleeper's pending signals
    /// were last examined.
    pub sig_epoch_seen: u64,
    /// The port ffs chose for a `bind` to port 0 in progress (see
    /// `Session::net_pre_syscall`), and how often it was retried.
    pub bound_port: Option<u16>,
    pub port_tries: u8,
    /// Replay: address of the execute breakpoint armed in `EXEC_SLOT`.
    /// Disarmed by the next `resume` unless `keep_exec_bp` is set (by the
    /// loop that hunts for a tick target).
    pub exec_bp: Option<u64>,
    pub keep_exec_bp: bool,
    /// The session's watchpoints at the time they were applied to this
    /// task (needed to program DR7 around the execute breakpoint).
    pub watch_addrs: Vec<u64>,
    /// The last resume was a `PTRACE_SINGLESTEP`.
    stepping: bool,
    /// The instruction being single-stepped is a `pushf`; this is the
    /// address of the next instruction (see `after_single_step`).
    pushf_target: Option<u64>,
}

/// The x86 trap flag.
pub const X86_TF: u64 = 1 << 8;

impl Task {
    pub fn new(tid: pid_t, tgid: pid_t, ltid: Ltid, pmu: &PmuConfig) -> Result<Task> {
        let hpc = TicksCounter::open(pmu, tid)?;
        Ok(Task {
            tid,
            tgid,
            ltid,
            state: State::Stopped(Stop::Parked),
            mem: Mem::open(tid),
            hpc,
            ticks: 0,
            regs: None,
            pending_sig: 0,
            in_syscall: false,
            syscall_nr: 0,
            interrupt_pending: false,
            execs: 0,
            exit_code: None,
            vdso_patched: false,
            tsc_mode: libc::PR_TSC_ENABLE,
            unreported: false,
            last_child: None,
            restarting: false,
            rseq: None,
            emulated: None,
            vblock: None,
            vdeadline: None,
            entry_regs: None,
            rewound: false,
            vwait: None,
            mem_restore: None,
            exit_status: None,
            exit_reported: false,
            block_class: BlockClass::Never,
            sig_epoch_seen: 0,
            bound_port: None,
            port_tries: 0,
            exec_bp: None,
            keep_exec_bp: false,
            watch_addrs: Vec::new(),
            stepping: false,
            pushf_target: None,
        })
    }

    pub fn is_stopped(&self) -> bool {
        matches!(self.state, State::Stopped(_))
    }

    /// In a ptrace stop (so registers and memory can be touched): either
    /// a scheduler-visible stop or a virtual block.
    pub fn is_ptrace_stopped(&self) -> bool {
        self.is_stopped() || self.vblock.is_some()
    }

    pub fn is_alive(&self) -> bool {
        self.state != State::Exited
    }

    pub fn stop(&self) -> Option<Stop> {
        match self.state {
            State::Stopped(s) => Some(s),
            _ => None,
        }
    }

    pub fn regs(&mut self) -> Result<Regs> {
        if let Some(r) = self.regs {
            return Ok(r);
        }
        if !self.is_ptrace_stopped() {
            bail!("regs of task {} requested while {:?}", self.tid, self.state);
        }
        let r = ptrace::get_regs(self.tid)?;
        self.regs = Some(r);
        Ok(r)
    }

    pub fn set_regs(&mut self, regs: &Regs) -> Result<()> {
        ptrace::set_regs(self.tid, regs)?;
        self.regs = Some(*regs);
        Ok(())
    }

    pub fn invalidate_regs(&mut self) {
        self.regs = None;
    }

    pub fn rip(&mut self) -> u64 {
        self.regs().map(|r| r.rip).unwrap_or(0)
    }

    /// Resume the task.  `budget` is the number of ticks after which it should
    /// be interrupted (0 = unlimited).  The pending signal (if any) is
    /// delivered.
    pub fn resume(&mut self, how: ResumeHow, budget: u64) -> Result<()> {
        let Some(stop) = self.stop() else {
            bail!("resume of task {} while {:?}", self.tid, self.state);
        };
        if let Stop::SyscallEntry { nr } = stop {
            self.in_syscall = true;
            self.syscall_nr = nr;
        }
        let sig = std::mem::replace(&mut self.pending_sig, 0);
        if self.exec_bp.is_some() && !self.keep_exec_bp {
            crate::watch::disarm_exec_breakpoint(self.tid, &self.watch_addrs)?;
            self.exec_bp = None;
        }
        self.pushf_target = None;
        self.stepping = matches!(how, ResumeHow::SingleStep | ResumeHow::BlockStep);
        if self.stepping && sig == 0 {
            // A single-stepped `pushf` pushes the flags with the trap flag
            // the step itself set; a later `popf` would then re-enable
            // single-stepping behind the tracer's back (the kernel stops
            // attributing TF to the debugger across a popf) and the
            // program would die of SIGTRAP.  Remember where the pushed
            // word will be so the bit can be cleared after the step.
            let rip = self.rip();
            let mut insn = [0u8; 2];
            if self.mem.read(rip, &mut insn).is_ok() {
                if insn[0] == 0x9c {
                    self.pushf_target = Some(rip + 1);
                } else if insn == [0x66, 0x9c] {
                    self.pushf_target = Some(rip + 2);
                }
            }
        }
        self.hpc.start(budget)?;
        self.regs = None;
        ptrace::resume(self.tid, how, sig)?;
        self.state = State::Running;
        Ok(())
    }

    /// Undo what a single step leaked into the tracee's view: the trap flag
    /// in the word a `pushf` pushed, and a trap flag that became the
    /// tracee's own (a stepped `popf` restoring such a word, or a signal
    /// taken during the step; rr does the same in `Task::did_waitpid`).
    fn after_single_step(&mut self, stop: Stop) -> Result<()> {
        if matches!(stop, Stop::ExitEvent) {
            return Ok(());
        }
        let mut r = ptrace::get_regs(self.tid)?;
        if let Some(target) = self.pushf_target.take() {
            if r.rip == target {
                let mut w = [0u8; 2];
                if self.mem.read(r.rsp, &mut w).is_ok() {
                    let v = u16::from_ne_bytes(w) & !(X86_TF as u16);
                    self.mem.write(r.rsp, &v.to_ne_bytes())?;
                }
            }
        }
        if r.eflags & X86_TF != 0 {
            log::debug!("task {}: clearing leaked trap flag at {:#x}", self.tid, r.rip);
            r.eflags &= !X86_TF;
            ptrace::set_regs(self.tid, &r)?;
        }
        self.regs = Some(r);
        Ok(())
    }

    /// The kernel reported this task's death through a wait outside the
    /// usual paths (a remote syscall): record it and let the scheduler
    /// report the exit.
    pub fn note_death(&mut self, st: WaitStatus) {
        let _ = self.hpc.stop();
        self.state = State::Exited;
        match st {
            WaitStatus::Exited(code) => {
                self.exit_code = Some(code);
                self.exit_status = Some((code & 0xff) << 8);
            }
            WaitStatus::Signaled(sig) => {
                self.exit_code = Some(128 + sig);
                self.exit_status = Some(sig);
            }
            _ => {}
        }
        self.unreported = true;
    }

    /// Record that a wait status was observed for this task.  Updates ticks,
    /// registers and state.  Returns the decoded stop (None if exited).
    pub fn did_wait(&mut self, status: WaitStatus) -> Result<Option<Stop>> {
        let delta = self.hpc.stop()?;
        self.ticks += delta;
        self.regs = None;
        let stop = match status {
            WaitStatus::Exited(code) => {
                self.state = State::Exited;
                self.exit_code = Some(code);
                self.exit_status = Some((code & 0xff) << 8);
                return Ok(None);
            }
            WaitStatus::Signaled(sig) => {
                self.state = State::Exited;
                self.exit_code = Some(128 + sig);
                self.exit_status = Some(sig);
                return Ok(None);
            }
            WaitStatus::SyscallStop => {
                let r = ptrace::get_regs(self.tid)?;
                self.regs = Some(r);
                if self.in_syscall {
                    self.in_syscall = false;
                    Stop::SyscallExit {
                        nr: self.syscall_nr,
                        ret: r.rax as i64,
                    }
                } else {
                    ptrace::check_syscall_abi(self.tid, r.orig_rax)?;
                    Stop::SyscallEntry { nr: r.orig_rax }
                }
            }
            WaitStatus::Event(ev) => match ev {
                libc::PTRACE_EVENT_CLONE | libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK => {
                    let child = ptrace::get_event_msg(self.tid)? as pid_t;
                    Stop::CloneEvent { child, event: ev }
                }
                libc::PTRACE_EVENT_EXEC => {
                    self.execs += 1;
                    // execve never returns to the old image; the kernel still
                    // delivers an exit stop for the execve syscall itself.
                    Stop::ExecEvent
                }
                libc::PTRACE_EVENT_EXIT => {
                    self.in_syscall = false;
                    Stop::ExitEvent
                }
                PTRACE_EVENT_STOP => {
                    self.interrupt_pending = false;
                    Stop::Parked
                }
                other => bail!("unexpected ptrace event {other} on task {}", self.tid),
            },
            WaitStatus::GroupStop(_sig) => {
                // A group-stop (SIGSTOP/SIGTSTP...).  Treat as parked; we never
                // actually let the tracee stop.
                Stop::Parked
            }
            WaitStatus::Signal(sig) => Stop::Signal { sig },
        };
        if self.stepping {
            self.stepping = false;
            self.after_single_step(stop)?;
        }
        self.state = State::Stopped(stop);
        Ok(Some(stop))
    }

    /// If this task is at a syscall-entry stop, abort the syscall (the kernel
    /// returns -ENOSYS) and rewind so that the next resume re-executes the
    /// syscall instruction.  Afterwards the task is `Parked`.  This is how we
    /// normalize state before cloning a task: the clone will re-enter the
    /// syscall from scratch.
    pub fn park_from_entry(&mut self) -> Result<()> {
        let Some(Stop::SyscallEntry { nr }) = self.stop() else {
            return Ok(());
        };
        let mut r = self.regs()?;
        r.orig_rax = u64::MAX; // -1: skip the syscall
        ptrace::set_regs(self.tid, &r)?;
        self.in_syscall = true;
        self.syscall_nr = nr;
        self.regs = None;
        ptrace::resume(self.tid, ResumeHow::Syscall, 0)?;
        self.state = State::Running;
        loop {
            let Some(st) = ptrace::wait_tid(self.tid)? else {
                bail!("task {} vanished while parking", self.tid);
            };
            let stop = self.did_wait(st)?;
            match stop {
                Some(Stop::SyscallExit { .. }) => break,
                Some(Stop::Parked) => continue, // late PTRACE_INTERRUPT stop
                Some(Stop::ExitEvent) | None => {
                    bail!("task {} died while parking", self.tid);
                }
                other => bail!("unexpected stop {other:?} while parking task {}", self.tid),
            }
        }
        let mut r = self.regs()?;
        r.rip -= SYSCALL_INSN_LEN;
        r.rax = nr;
        r.orig_rax = nr;
        self.set_regs(&r)?;
        self.state = State::Stopped(Stop::Parked);
        self.rewound = true;
        Ok(())
    }

    /// Called at a PTRACE_INTERRUPT (or other) stop where the registers show
    /// an interrupted restartable syscall: rewind to re-execute the syscall on
    /// the next resume so the kernel's own restart logic is not needed.
    pub fn rewind_interrupted_syscall(&mut self) -> Result<bool> {
        let r = self.regs()?;
        if (r.orig_rax as i64) >= 0 && ptrace::is_restart_result(r.rax) {
            let mut n = r;
            n.rip -= SYSCALL_INSN_LEN;
            n.rax = r.orig_rax;
            self.set_regs(&n)?;
            self.in_syscall = false;
            self.rewound = true;
            return Ok(true);
        }
        Ok(false)
    }
}
