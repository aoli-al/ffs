//! Injecting syscalls into a stopped tracee (rr's `AutoRemoteSyscalls`).
//!
//! The task's registers are saved, the instruction pointer is moved to a
//! `syscall` instruction (we use the one inside the vDSO, so no code has to
//! be patched), the syscall number and arguments are loaded into registers
//! and the task is resumed until the syscall completes.  Afterwards the
//! original registers are restored, so the tracee never notices.

use anyhow::{anyhow, bail, Context, Result};
use libc::pid_t;

use crate::mem::read_maps;
use crate::ptrace::{self, Regs, ResumeHow, WaitStatus, SYSCALL_INSN};
use crate::task::{State, Stop, Task};

/// The tracee died (its process was killed) while a syscall was being
/// injected into it.  The status is recorded on the task; callers acting
/// on the tracee's behalf treat this as "the task is gone" and let the
/// scheduler report the exit.
#[derive(Debug)]
pub struct TaskDied(pub pid_t);

impl std::fmt::Display for TaskDied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "task {} died during remote syscall", self.0)
    }
}

impl std::error::Error for TaskDied {}

pub struct Remote<'a> {
    pub task: &'a mut Task,
    saved_regs: Regs,
    saved_state: State,
    saved_pending_sig: i32,
    saved_ticks: u64,
    saved_in_syscall: (bool, u64),
    syscall_ip: u64,
    /// Bytes we overwrote to create a syscall instruction (if no vDSO).
    patched: Option<(u64, [u8; 2])>,
    /// Scratch memory on the tracee stack: (addr, saved bytes).
    scratch: Vec<(u64, Vec<u8>)>,
    scratch_top: u64,
    /// Child tids produced by injected clone/fork syscalls.
    pub children: Vec<pid_t>,
    restored: bool,
}

/// Find a `syscall` instruction in the tracee's vDSO.
pub fn find_vdso_syscall(tid: pid_t, mem: &crate::mem::Mem) -> Option<u64> {
    let maps = read_maps(tid).ok()?;
    let vdso = maps.iter().find(|m| m.path == "[vdso]")?;
    let size = (vdso.end - vdso.start) as usize;
    let mut buf = vec![0u8; size];
    mem.read(vdso.start, &mut buf).ok()?;
    buf.windows(2)
        .position(|w| w == SYSCALL_INSN)
        .map(|p| vdso.start + p as u64)
}

impl<'a> Remote<'a> {
    /// Prepare `task` (which must be stopped) for remote syscalls.  A task at
    /// a syscall-entry stop is first parked (its syscall is aborted and will
    /// be re-executed when the task is resumed later).
    pub fn new(task: &'a mut Task) -> Result<Remote<'a>> {
        if !task.is_ptrace_stopped() {
            bail!(
                "remote syscall on task {} which is {:?}",
                task.tid,
                task.state
            );
        }
        task.park_from_entry()?;
        let saved_regs = task.regs()?;
        let saved_state = task.state;
        let saved_pending_sig = std::mem::replace(&mut task.pending_sig, 0);
        let saved_ticks = task.ticks;
        let saved_in_syscall = (task.in_syscall, task.syscall_nr);
        let mut patched = None;
        let syscall_ip = match find_vdso_syscall(task.tid, &task.mem) {
            Some(ip) => ip,
            None => {
                // No vDSO: patch a syscall instruction at the current rip.
                let mut old = [0u8; 2];
                task.mem.read(saved_regs.rip, &mut old)?;
                task.mem.write(saved_regs.rip, &SYSCALL_INSN)?;
                patched = Some((saved_regs.rip, old));
                saved_regs.rip
            }
        };
        let scratch_top = (saved_regs.rsp - 512) & !15;
        Ok(Remote {
            task,
            saved_regs,
            saved_state,
            saved_pending_sig,
            saved_ticks,
            saved_in_syscall,
            syscall_ip,
            patched,
            scratch: Vec::new(),
            scratch_top,
            children: Vec::new(),
            restored: false,
        })
    }

    /// Copy `data` into scratch space on the tracee's stack; returns its address.
    pub fn push_scratch(&mut self, data: &[u8]) -> Result<u64> {
        let len = (data.len() + 15) & !15;
        self.scratch_top -= len as u64;
        let addr = self.scratch_top;
        let mut old = vec![0u8; len];
        self.task
            .mem
            .read(addr, &mut old)
            .context("read scratch area")?;
        self.task
            .mem
            .write(addr, data)
            .context("write scratch area")?;
        self.scratch.push((addr, old));
        Ok(addr)
    }

    /// Reserve zeroed scratch space.
    pub fn alloc_scratch(&mut self, len: usize) -> Result<u64> {
        self.push_scratch(&vec![0u8; len])
    }

    /// Execute `nr(args...)` in the tracee and return its result.
    pub fn syscall(&mut self, nr: u64, args: &[u64]) -> Result<i64> {
        let tid = self.task.tid;
        let mut r = self.saved_regs;
        r.rip = self.syscall_ip;
        r.rax = nr;
        r.orig_rax = nr;
        let regs = [
            &mut r.rdi, &mut r.rsi, &mut r.rdx, &mut r.r10, &mut r.r8, &mut r.r9,
        ];
        for (i, slot) in regs.into_iter().enumerate() {
            *slot = args.get(i).copied().unwrap_or(0);
        }
        self.task.set_regs(&r)?;
        self.task.in_syscall = false;
        // Run to the syscall-entry stop.
        self.task.state = State::Stopped(Stop::Parked);
        loop {
            ptrace::resume(tid, ResumeHow::Syscall, 0)?;
            let st = ptrace::wait_tid(tid)?
                .ok_or_else(|| anyhow!("task {tid} vanished during remote syscall"))?;
            match st {
                WaitStatus::SyscallStop => {
                    let cur = ptrace::get_regs(tid)?;
                    if cur.orig_rax != nr || cur.rip != self.syscall_ip + 2 {
                        bail!(
                            "remote syscall: unexpected syscall stop (nr {} at {:#x})",
                            cur.orig_rax,
                            cur.rip
                        );
                    }
                    break;
                }
                WaitStatus::Event(ptrace::PTRACE_EVENT_STOP) | WaitStatus::GroupStop(_) => continue,
                WaitStatus::Signal(sig) => {
                    log::debug!("remote syscall: deferring signal {sig} on task {tid}");
                    // Leave it pending in the kernel by not delivering now; it
                    // will be re-raised when the task is next resumed for real.
                    continue;
                }
                WaitStatus::Exited(_) | WaitStatus::Signaled(_) => {
                    self.task.note_death(st);
                    return Err(TaskDied(tid).into());
                }
                other => bail!("remote syscall: unexpected status {other:?}"),
            }
        }
        // Run to the syscall-exit stop, collecting clone events on the way.
        loop {
            ptrace::resume(tid, ResumeHow::Syscall, 0)?;
            let st = ptrace::wait_tid(tid)?
                .ok_or_else(|| anyhow!("task {tid} vanished during remote syscall"))?;
            match st {
                WaitStatus::SyscallStop => break,
                WaitStatus::Event(ev)
                    if ev == libc::PTRACE_EVENT_CLONE
                        || ev == libc::PTRACE_EVENT_FORK
                        || ev == libc::PTRACE_EVENT_VFORK =>
                {
                    let child = ptrace::get_event_msg(tid)? as pid_t;
                    self.children.push(child);
                }
                WaitStatus::Event(ptrace::PTRACE_EVENT_STOP) | WaitStatus::GroupStop(_) => continue,
                WaitStatus::Signal(_) => continue,
                WaitStatus::Exited(_) | WaitStatus::Signaled(_) => {
                    self.task.note_death(st);
                    return Err(TaskDied(tid).into());
                }
                other => bail!("remote syscall: unexpected status {other:?}"),
            }
        }
        let out = ptrace::get_regs(tid)?;
        self.task.invalidate_regs();
        Ok(out.rax as i64)
    }

    /// Like `syscall` but fails on a negative (errno) result.
    pub fn infallible(&mut self, name: &str, nr: u64, args: &[u64]) -> Result<i64> {
        let r = self.syscall(nr, args)?;
        if (-4095..0).contains(&r) {
            bail!(
                "remote {name} failed in task {}: {}",
                self.task.tid,
                std::io::Error::from_raw_os_error(-r as i32)
            );
        }
        Ok(r)
    }

    /// Restore the task to its original state.  Called automatically on drop.
    pub fn restore(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        self.restored = true;
        if !self.task.is_alive() {
            return Ok(());
        }
        for (addr, old) in self.scratch.drain(..).rev() {
            self.task.mem.write(addr, &old)?;
        }
        if let Some((addr, old)) = self.patched.take() {
            self.task.mem.write(addr, &old)?;
        }
        self.task.set_regs(&self.saved_regs)?;
        self.task.state = self.saved_state;
        self.task.pending_sig = self.saved_pending_sig;
        self.task.ticks = self.saved_ticks;
        self.task.in_syscall = self.saved_in_syscall.0;
        self.task.syscall_nr = self.saved_in_syscall.1;
        Ok(())
    }

    /// Restore with different registers/state than the ones saved (used for
    /// a freshly forked child, whose registers must become the parent's).
    pub fn restore_to(&mut self, regs: Regs, state: State) -> Result<()> {
        self.saved_regs = regs;
        self.saved_state = state;
        self.restore()
    }
}

impl Drop for Remote<'_> {
    fn drop(&mut self) {
        if let Err(e) = self.restore() {
            log::error!(
                "failed to restore task {} after remote syscall: {e:#}",
                self.task.tid
            );
        }
    }
}

// Syscall numbers used by the checkpoint code (x86-64).
pub const SYS_CLOSE: u64 = 3;
pub const SYS_LSEEK: u64 = 8;
pub const SYS_CLONE: u64 = 56;
pub const SYS_SIGALTSTACK: u64 = 131;
pub const SYS_PRCTL: u64 = 157;
pub const SYS_SET_TID_ADDRESS: u64 = 218;
pub const SYS_OPENAT: u64 = 257;
pub const SYS_SET_ROBUST_LIST: u64 = 273;
pub const SYS_GET_ROBUST_LIST: u64 = 274;
pub const SYS_DUP3: u64 = 292;
pub const SYS_MMAP: u64 = 9;
pub const SYS_EVENTFD2: u64 = 290;
pub const SYS_RSEQ: u64 = 334;
pub const SYS_FCNTL: u64 = 72;
