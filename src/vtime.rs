//! Virtual time, rr-style.  Under `--virtual-time` (the default) the tracee
//! never observes real time: clocks and `rdtsc` are derived from the number
//! of ticks executed (see `session.rs`), and everything that *waits* for
//! time is driven by that clock instead of the kernel's:
//!
//! * `nanosleep`/`clock_nanosleep` are not executed at all.  The thread is
//!   parked by the tracer (a "virtual block") until the virtual clock
//!   reaches the deadline, or a signal becomes pending (then `EINTR`).
//! * timeouts of blocking syscalls (`futex`, `poll`, `ppoll`, `select`,
//!   `pselect6`, `epoll_wait`, `epoll_pwait[2]`, `rt_sigtimedwait`,
//!   `futex_waitv`) are stripped before the kernel sees the call; the
//!   tracer remembers the virtual deadline and, when the clock passes it
//!   while the thread is still blocked, interrupts the syscall and gives it
//!   the timeout result.
//! * `setitimer`/`alarm`/`timer_*` are emulated: timers fire at scheduling
//!   points when the virtual clock passes their deadline; the signal is sent
//!   with `tgkill` to a deterministically chosen thread and its `siginfo`
//!   rewritten to what the kernel would have produced.
//! * When no thread can run and some deadline is pending, the clock jumps
//!   to the earliest deadline (a discrete-event simulation), so sleeping
//!   costs nothing and programs behave identically on every run.
//!
//! Everything here is a function of the schedule, so a replay (and every
//! branch of an exploration) makes the same decisions at the same steps.

use anyhow::{bail, Result};
use libc::pid_t;

use crate::ptrace;
use crate::session::{Event, Session};
use crate::task::{Ltid, State, Stop, Task};

pub const NS_PER_SEC: u64 = 1_000_000_000;

const NANOSLEEP: u64 = 35;
const GETITIMER: u64 = 36;
const ALARM: u64 = 37;
const SETITIMER: u64 = 38;
const CLOCK_NANOSLEEP: u64 = 230;
const TIMER_CREATE: u64 = 222;
const TIMER_SETTIME: u64 = 223;
const TIMER_GETTIME: u64 = 224;
const TIMER_GETOVERRUN: u64 = 225;
const TIMER_DELETE: u64 = 226;
const TIMERFD_CREATE: u64 = 283;
const TIMERFD_SETTIME: u64 = 286;
const TIMERFD_GETTIME: u64 = 287;
const CLOSE: u64 = 3;
const FUTEX: u64 = 202;
const FUTEX_WAITV: u64 = 449;
const POLL: u64 = 7;
const SELECT: u64 = 23;
const RT_SIGTIMEDWAIT: u64 = 128;
const EPOLL_WAIT: u64 = 232;
const PSELECT6: u64 = 270;
const PPOLL: u64 = 271;
const EPOLL_PWAIT: u64 = 281;
const EPOLL_PWAIT2: u64 = 441;

const FUTEX_WAIT: u64 = 0;
const FUTEX_LOCK_PI: u64 = 6;
const FUTEX_WAIT_BITSET: u64 = 9;
const FUTEX_WAIT_REQUEUE_PI: u64 = 11;
const FUTEX_LOCK_PI2: u64 = 13;
const FUTEX_CMD_MASK: u64 = !(128 | 256);
const FUTEX_CLOCK_REALTIME: u64 = 256;

const CLOCK_REALTIME: u64 = 0;
const CLOCK_REALTIME_COARSE: u64 = 5;
const TIMER_ABSTIME: u64 = 1;

const ITIMER_REAL: u64 = 0;
const ITIMER_VIRTUAL: u64 = 1;
const ITIMER_PROF: u64 = 2;

const SIGEV_SIGNAL: i32 = 0;
const SIGEV_NONE: i32 = 1;
const SIGEV_THREAD_ID: i32 = 4;

const SI_KERNEL: i32 = 0x80;
const SI_TIMER: i32 = -2;

/// A thread parked by the tracer instead of the kernel: its syscall was
/// aborted and the tracer supplies the result when the block ends.
#[derive(Clone, Copy, Debug)]
pub struct VBlock {
    pub nr: u64,
    /// Virtual time at which the block ends by itself.
    pub deadline: Option<u64>,
    /// Address of a `timespec` that receives the remaining time on `EINTR`.
    pub rem: u64,
}

/// Decision of `pre_syscall` for an intercepted syscall.
#[derive(Clone, Debug)]
pub enum Emulation {
    /// The syscall is skipped; this is its result.
    Ret(i64),
    /// The syscall is skipped and the thread blocks virtually.
    Block(VBlock),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimerKind {
    Itimer(u64),
    Posix { id: i32, clockid: u64, notify: i32 },
    /// A timerfd of the process: the kernel's timer stays disarmed, the
    /// deadline lives on the virtual clock, and when it arrives ffs arms
    /// the real timerfd for one immediate expiration so the descriptor
    /// becomes readable (and wakes epoll/poll/read on it) exactly then.
    TimerFd { fd: i32, clockid: u64 },
}

#[derive(Clone, Debug)]
pub struct VTimer {
    pub proc_key: Ltid,
    /// Thread to signal (`SIGEV_THREAD_ID`); otherwise process-directed.
    pub thread: Option<Ltid>,
    pub kind: TimerKind,
    pub sig: i32,
    pub sigval: u64,
    pub deadline: Option<u64>,
    pub interval: u64,
    pub overrun: i32,
    /// Expirations since the deadlock detector last saw progress.
    pub fires: u32,
}

fn read_ts(t: &Task, addr: u64) -> Result<u64> {
    let mut b = [0u8; 16];
    t.mem.read(addr, &mut b)?;
    let sec = i64::from_ne_bytes(b[..8].try_into().unwrap());
    let nsec = i64::from_ne_bytes(b[8..].try_into().unwrap());
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        bail!("invalid timespec");
    }
    Ok((sec as u64).saturating_mul(NS_PER_SEC).saturating_add(nsec as u64))
}

fn write_ts(t: &Task, addr: u64, ns: u64) -> Result<()> {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&((ns / NS_PER_SEC) as i64).to_ne_bytes());
    b[8..].copy_from_slice(&((ns % NS_PER_SEC) as i64).to_ne_bytes());
    t.mem.write(addr, &b)
}

fn read_tv(t: &Task, addr: u64) -> Result<u64> {
    let mut b = [0u8; 16];
    t.mem.read(addr, &mut b)?;
    let sec = i64::from_ne_bytes(b[..8].try_into().unwrap());
    let usec = i64::from_ne_bytes(b[8..].try_into().unwrap());
    if sec < 0 || !(0..1_000_000).contains(&usec) {
        bail!("invalid timeval");
    }
    Ok((sec as u64).saturating_mul(NS_PER_SEC).saturating_add(usec as u64 * 1000))
}

fn write_tv(t: &Task, addr: u64, ns: u64) -> Result<()> {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&((ns / NS_PER_SEC) as i64).to_ne_bytes());
    b[8..].copy_from_slice(&((ns % NS_PER_SEC / 1000) as i64).to_ne_bytes());
    t.mem.write(addr, &b)
}

use crate::ptrace::blocked_signals;

fn sigbit(sig: i32) -> u64 {
    1u64 << (sig - 1)
}

impl Session {
    /// Convert an absolute time of `clockid` as seen by the tracee into a
    /// virtual-clock value.
    fn abs_to_virtual(&self, clockid: u64, abs_ns: u64) -> u64 {
        match clockid {
            CLOCK_REALTIME | CLOCK_REALTIME_COARSE => abs_ns.saturating_sub(self.vclock.base_realtime_ns),
            _ => abs_ns.saturating_sub(self.vclock.base_monotonic_ns),
        }
    }

    /// Skip the syscall the task is entering and answer it from `em`.
    pub fn emulate_syscall(&mut self, tid: pid_t, em: Emulation) -> Result<()> {
        let t = self.task_mut(tid)?;
        let mut r = t.regs()?;
        r.orig_rax = u64::MAX;
        t.set_regs(&r)?;
        t.emulated = Some(em);
        Ok(())
    }

    /// Strip the timeout argument of a blocking syscall and remember the
    /// virtual deadline; `set` patches the registers.
    fn strip_timeout(
        &mut self,
        tid: pid_t,
        deadline: u64,
        timeout_ret: i64,
        set: impl Fn(&mut ptrace::Regs),
    ) -> Result<()> {
        let t = self.task_mut(tid)?;
        let r = t.regs()?;
        let mut n = r;
        set(&mut n);
        t.set_regs(&n)?;
        if t.entry_regs.is_none() {
            t.entry_regs = Some(r);
        }
        t.vdeadline = Some((deadline, timeout_ret));
        Ok(())
    }

    /// Intercept time-related syscalls at their entry stop.
    pub fn vt_pre_syscall(&mut self, tid: pid_t, nr: u64) -> Result<()> {
        let now = self.virtual_ns();
        let t = self.task_mut(tid)?;
        let r = t.regs()?;
        // A syscall re-executed after a rewind or a kernel restart keeps the
        // deadline it already had.
        let keep = (t.rewound || t.restarting) && t.vdeadline.is_some();
        t.rewound = false;
        let kept = t.vdeadline;
        match nr {
            NANOSLEEP | CLOCK_NANOSLEEP => {
                let (req, rem, absolute, clockid) = if nr == NANOSLEEP {
                    (r.rdi, r.rsi, false, 1)
                } else {
                    (r.rdx, r.r10, r.rsi & TIMER_ABSTIME != 0, r.rdi)
                };
                let Ok(ns) = read_ts(t, req) else {
                    return Ok(()); // let the kernel report the fault
                };
                let deadline = if absolute { self.abs_to_virtual(clockid, ns) } else { now.saturating_add(ns) };
                let rem = if absolute { 0 } else { rem };
                self.emulate_syscall(tid, Emulation::Block(VBlock { nr, deadline: Some(deadline), rem }))?;
            }
            FUTEX => {
                let op = r.rsi & FUTEX_CMD_MASK;
                let (absolute, clockid) = match op {
                    FUTEX_WAIT => (false, 1),
                    FUTEX_WAIT_BITSET | FUTEX_WAIT_REQUEUE_PI => {
                        (true, if r.rsi & FUTEX_CLOCK_REALTIME != 0 { CLOCK_REALTIME } else { 1 })
                    }
                    FUTEX_LOCK_PI => (true, CLOCK_REALTIME),
                    FUTEX_LOCK_PI2 => (true, if r.rsi & FUTEX_CLOCK_REALTIME != 0 { CLOCK_REALTIME } else { 1 }),
                    _ => return Ok(()),
                };
                if r.r10 == 0 {
                    return Ok(());
                }
                let deadline = if keep {
                    kept.unwrap().0
                } else {
                    let Ok(ns) = read_ts(t, r.r10) else { return Ok(()) };
                    if absolute { self.abs_to_virtual(clockid, ns) } else { now.saturating_add(ns) }
                };
                if deadline <= now {
                    return Ok(()); // the kernel times it out at once
                }
                self.strip_timeout(tid, deadline, -(libc::ETIMEDOUT as i64), |n| n.r10 = 0)?;
            }
            FUTEX_WAITV => {
                if r.rdx == 0 {
                    return Ok(());
                }
                let deadline = if keep {
                    kept.unwrap().0
                } else {
                    let Ok(ns) = read_ts(t, r.rdx) else { return Ok(()) };
                    self.abs_to_virtual(r.r8, ns)
                };
                if deadline <= now {
                    return Ok(());
                }
                self.strip_timeout(tid, deadline, -(libc::ETIMEDOUT as i64), |n| n.rdx = 0)?;
            }
            POLL | EPOLL_WAIT | EPOLL_PWAIT => {
                let (arg, set): (i32, fn(&mut ptrace::Regs)) = if nr == POLL {
                    (r.rdx as i32, |n| n.rdx = u32::MAX as u64)
                } else {
                    (r.r10 as i32, |n| n.r10 = u32::MAX as u64)
                };
                if arg <= 0 && !keep {
                    return Ok(());
                }
                let deadline = if keep { kept.unwrap().0 } else { now.saturating_add(arg as u64 * 1_000_000) };
                self.strip_timeout(tid, deadline, 0, set)?;
            }
            PPOLL | EPOLL_PWAIT2 | SELECT | PSELECT6 | RT_SIGTIMEDWAIT => {
                let (ptr, set, ret): (u64, fn(&mut ptrace::Regs), i64) = match nr {
                    PPOLL => (r.rdx, |n| n.rdx = 0, 0),
                    EPOLL_PWAIT2 => (r.r10, |n| n.r10 = 0, 0),
                    SELECT | PSELECT6 => (r.r8, |n| n.r8 = 0, 0),
                    _ => (r.rdx, |n| n.rdx = 0, -(libc::EAGAIN as i64)),
                };
                if ptr == 0 && !keep {
                    return Ok(());
                }
                let deadline = if keep {
                    kept.unwrap().0
                } else {
                    let read = if nr == SELECT { read_tv(t, ptr) } else { read_ts(t, ptr) };
                    let Ok(ns) = read else { return Ok(()) };
                    if ns == 0 {
                        return Ok(()); // a poll: the kernel returns at once
                    }
                    now.saturating_add(ns)
                };
                self.strip_timeout(tid, deadline, ret, set)?;
            }
            SETITIMER | GETITIMER | ALARM | TIMER_CREATE | TIMER_SETTIME | TIMER_GETTIME
            | TIMER_GETOVERRUN | TIMER_DELETE | TIMERFD_GETTIME => {
                let ret = self.vt_timer_syscall(tid, nr)?;
                self.emulate_syscall(tid, Emulation::Ret(ret))?;
            }
            TIMERFD_SETTIME => self.vt_timerfd_settime(tid)?,
            _ => {}
        }
        Ok(())
    }

    /// `timerfd_settime`: take the deadline onto the virtual clock and let
    /// the kernel see a disarming call (all-zero `new_value`, restored
    /// after the syscall), so the real timer never fires on its own.
    fn vt_timerfd_settime(&mut self, tid: pid_t) -> Result<()> {
        let now = self.virtual_ns();
        let key = self.proc_key(tid)?;
        let t = self.task_mut(tid)?;
        let r = t.regs()?;
        let fd = r.rdi as i32;
        if r.rdx == 0 || t.mem_restore.is_some() {
            return Ok(());
        }
        let Some(idx) = self
            .vtimers
            .iter()
            .position(|tm| tm.proc_key == key && matches!(tm.kind, TimerKind::TimerFd { fd: f, .. } if f == fd))
        else {
            return Ok(()); // not a timerfd we know (EINVAL/EBADF from the kernel)
        };
        let t = self.task_mut(tid)?;
        let (Ok(interval), Ok(value)) = (read_ts(t, r.rdx), read_ts(t, r.rdx + 16)) else { return Ok(()) };
        let clockid = match self.vtimers[idx].kind {
            TimerKind::TimerFd { clockid, .. } => clockid,
            _ => 1,
        };
        let deadline = if value == 0 {
            None
        } else if r.rsi & TIMER_ABSTIME != 0 {
            Some(self.abs_to_virtual(clockid, value).max(now))
        } else {
            Some(now + value)
        };
        {
            let tm = &mut self.vtimers[idx];
            tm.interval = interval;
            tm.deadline = deadline;
            tm.overrun = 0;
        }
        let t = self.task_mut(tid)?;
        let mut old = [0u8; 32];
        t.mem.read(r.rdx, &mut old)?;
        t.mem.write(r.rdx, &[0u8; 32])?;
        t.mem_restore = Some((r.rdx, old.to_vec()));
        Ok(())
    }

    /// Bookkeeping at the exit of syscalls that create or close timerfds.
    pub fn vt_timerfd_exit(&mut self, tid: pid_t, nr: u64) -> Result<()> {
        let key = self.proc_key(tid)?;
        let r = self.task_mut(tid)?.regs()?;
        match nr {
            TIMERFD_CREATE if (r.rax as i64) >= 0 => {
                let fd = r.rax as i32;
                self.vtimers.retain(|tm| !(tm.proc_key == key && matches!(tm.kind, TimerKind::TimerFd { fd: f, .. } if f == fd)));
                self.vtimers.push(VTimer {
                    proc_key: key,
                    thread: None,
                    kind: TimerKind::TimerFd { fd, clockid: r.rdi },
                    sig: 0,
                    sigval: 0,
                    deadline: None,
                    interval: 0,
                    overrun: 0,
                    fires: 0,
                });
            }
            CLOSE if r.rax == 0 => {
                let fd = r.rdi as i32;
                self.vtimers.retain(|tm| !(tm.proc_key == key && matches!(tm.kind, TimerKind::TimerFd { fd: f, .. } if f == fd)));
            }
            _ => {}
        }
        Ok(())
    }

    /// Restore the argument registers of a syscall whose timeout was
    /// stripped (the program may look at them), once it is over.
    pub fn vt_syscall_done(&mut self, tid: pid_t) -> Result<()> {
        let now = self.virtual_ns();
        let t = self.task_mut(tid)?;
        let deadline = t.vdeadline.take().map(|(d, _)| d);
        if let Some((addr, bytes)) = t.mem_restore.take() {
            t.mem.write(addr, &bytes)?;
        }
        t.bound_port = None;
        if let (SELECT | PPOLL | PSELECT6, Some(e), Some(d)) = (t.syscall_nr, t.entry_regs, deadline) {
            // The raw select, pselect6 and ppoll syscalls update their
            // timeout buffers (libc may hide this with a temporary copy).
            let addr = if t.syscall_nr == PPOLL { e.rdx } else { e.r8 };
            if addr != 0 {
                let left = d.saturating_sub(now);
                let divisor = if t.syscall_nr == SELECT { 1000 } else { 1 };
                t.mem.write_u64(addr, left / 1_000_000_000)?;
                t.mem.write_u64(addr + 8, (left % 1_000_000_000) / divisor)?;
            }
        }
        if let Some(e) = t.entry_regs.take() {
            let mut r = t.regs()?;
            r.rdi = e.rdi;
            r.rsi = e.rsi;
            r.rdx = e.rdx;
            r.r10 = e.r10;
            r.r8 = e.r8;
            r.r9 = e.r9;
            t.set_regs(&r)?;
        }
        Ok(())
    }

    fn vt_timer_syscall(&mut self, tid: pid_t, nr: u64) -> Result<i64> {
        let now = self.virtual_ns();
        let key = self.proc_key(tid)?;
        let t = self.task_mut(tid)?;
        let r = t.regs()?;
        let remaining = |tm: &VTimer| tm.deadline.map(|d| d.saturating_sub(now)).unwrap_or(0);
        match nr {
            SETITIMER | GETITIMER | ALARM => {
                let which = if nr == ALARM { ITIMER_REAL } else { r.rdi };
                if !matches!(which, ITIMER_REAL | ITIMER_VIRTUAL | ITIMER_PROF) {
                    return Ok(-(libc::EINVAL as i64));
                }
                let (value, interval) = match nr {
                    SETITIMER => {
                        if r.rsi == 0 {
                            return Ok(-(libc::EFAULT as i64));
                        }
                        let interval = read_tv(t, r.rsi)?;
                        let value = read_tv(t, r.rsi + 16)?;
                        (value, interval)
                    }
                    ALARM => (r.rdi * NS_PER_SEC, 0),
                    _ => (0, 0),
                };
                let old = self
                    .vtimers
                    .iter()
                    .find(|tm| tm.proc_key == key && tm.kind == TimerKind::Itimer(which))
                    .map(|tm| (remaining(tm), tm.interval))
                    .unwrap_or((0, 0));
                let t = self.task_mut(tid)?;
                match nr {
                    SETITIMER if r.rdx != 0 => {
                        write_tv(t, r.rdx, old.1)?;
                        write_tv(t, r.rdx + 16, old.0)?;
                    }
                    GETITIMER => {
                        if r.rsi == 0 {
                            return Ok(-(libc::EFAULT as i64));
                        }
                        write_tv(t, r.rsi, old.1)?;
                        write_tv(t, r.rsi + 16, old.0)?;
                        return Ok(0);
                    }
                    _ => {}
                }
                self.vtimers.retain(|tm| !(tm.proc_key == key && tm.kind == TimerKind::Itimer(which)));
                if value != 0 {
                    self.vtimers.push(VTimer {
                        proc_key: key,
                        thread: None,
                        kind: TimerKind::Itimer(which),
                        sig: match which {
                            ITIMER_REAL => libc::SIGALRM,
                            ITIMER_VIRTUAL => libc::SIGVTALRM,
                            _ => libc::SIGPROF,
                        },
                        sigval: 0,
                        deadline: Some(now + value),
                        interval,
                        overrun: 0,
                        fires: 0,
                    });
                }
                Ok(if nr == ALARM { old.0.div_ceil(NS_PER_SEC) as i64 } else { 0 })
            }
            TIMER_CREATE => {
                if r.rdx == 0 {
                    return Ok(-(libc::EFAULT as i64));
                }
                let (sigval, sig, notify, tid_arg) = if r.rsi != 0 {
                    let mut b = [0u8; 20];
                    t.mem.read(r.rsi, &mut b)?;
                    (
                        u64::from_ne_bytes(b[..8].try_into().unwrap()),
                        i32::from_ne_bytes(b[8..12].try_into().unwrap()),
                        i32::from_ne_bytes(b[12..16].try_into().unwrap()),
                        i32::from_ne_bytes(b[16..20].try_into().unwrap()),
                    )
                } else {
                    (0, libc::SIGALRM, SIGEV_SIGNAL, 0)
                };
                if !matches!(notify, SIGEV_SIGNAL | SIGEV_NONE | SIGEV_THREAD_ID) || (notify != SIGEV_NONE && !(1..=64).contains(&sig)) {
                    return Ok(-(libc::EINVAL as i64));
                }
                let thread = if notify == SIGEV_THREAD_ID {
                    match self.ltid_of_tracee_pid(tid_arg as pid_t) {
                        Some(l) => Some(l),
                        None => return Ok(-(libc::EINVAL as i64)),
                    }
                } else {
                    None
                };
                let mut id = 0;
                while self.vtimers.iter().any(|tm| tm.proc_key == key && matches!(tm.kind, TimerKind::Posix { id: i, .. } if i == id)) {
                    id += 1;
                }
                let sigval = if r.rsi != 0 { sigval } else { id as u64 };
                self.vtimers.push(VTimer {
                    proc_key: key,
                    thread,
                    kind: TimerKind::Posix { id, clockid: r.rdi, notify },
                    sig,
                    sigval,
                    deadline: None,
                    interval: 0,
                    overrun: 0,
                    fires: 0,
                });
                let t = self.task_mut(tid)?;
                t.mem.write(r.rdx, &id.to_ne_bytes())?;
                Ok(0)
            }
            TIMER_SETTIME | TIMER_GETTIME | TIMER_GETOVERRUN | TIMER_DELETE => {
                let want = r.rdi as i32;
                let Some(idx) = self
                    .vtimers
                    .iter()
                    .position(|tm| tm.proc_key == key && matches!(tm.kind, TimerKind::Posix { id, .. } if id == want))
                else {
                    return Ok(-(libc::EINVAL as i64));
                };
                let old = (remaining(&self.vtimers[idx]), self.vtimers[idx].interval);
                match nr {
                    TIMER_GETOVERRUN => Ok(self.vtimers[idx].overrun as i64),
                    TIMER_DELETE => {
                        self.vtimers.remove(idx);
                        Ok(0)
                    }
                    TIMER_GETTIME => {
                        if r.rsi == 0 {
                            return Ok(-(libc::EFAULT as i64));
                        }
                        let t = self.task_mut(tid)?;
                        write_ts(t, r.rsi, old.1)?;
                        write_ts(t, r.rsi + 16, old.0)?;
                        Ok(0)
                    }
                    _ => {
                        if r.rdx == 0 {
                            return Ok(-(libc::EFAULT as i64));
                        }
                        let t = self.task_mut(tid)?;
                        let interval = read_ts(t, r.rdx)?;
                        let value = read_ts(t, r.rdx + 16)?;
                        if r.r10 != 0 {
                            write_ts(t, r.r10, old.1)?;
                            write_ts(t, r.r10 + 16, old.0)?;
                        }
                        let clockid = match self.vtimers[idx].kind {
                            TimerKind::Posix { clockid, .. } => clockid,
                            _ => 1,
                        };
                        let deadline = if value == 0 {
                            None
                        } else if r.rsi & TIMER_ABSTIME != 0 {
                            Some(self.abs_to_virtual(clockid, value).max(now))
                        } else {
                            Some(now + value)
                        };
                        let tm = &mut self.vtimers[idx];
                        tm.interval = interval;
                        tm.deadline = deadline;
                        Ok(0)
                    }
                }
            }
            TIMERFD_GETTIME => {
                let want = r.rdi as i32;
                let Some(idx) = self
                    .vtimers
                    .iter()
                    .position(|tm| tm.proc_key == key && matches!(tm.kind, TimerKind::TimerFd { fd, .. } if fd == want))
                else {
                    return Ok(-(libc::EINVAL as i64));
                };
                if r.rsi == 0 {
                    return Ok(-(libc::EFAULT as i64));
                }
                let left = remaining(&self.vtimers[idx]);
                let interval = self.vtimers[idx].interval;
                let t = self.task_mut(tid)?;
                write_ts(t, r.rsi, interval)?;
                write_ts(t, r.rsi + 16, left)?;
                Ok(0)
            }
            _ => unreachable!(),
        }
    }

    /// Fire every virtual timer whose deadline has passed.
    fn vt_fire_timers(&mut self) -> Result<()> {
        let now = self.virtual_ns();
        let mut i = 0;
        while i < self.vtimers.len() {
            let Some(deadline) = self.vtimers[i].deadline else {
                i += 1;
                continue;
            };
            if deadline > now {
                i += 1;
                continue;
            }
            let (proc_key, thread, sig, kind, sigval, interval) = {
                let tm = &self.vtimers[i];
                (tm.proc_key, tm.thread, tm.sig, tm.kind.clone(), tm.sigval, tm.interval)
            };
            // Re-arm (counting missed periods as overruns) or disarm.
            let late_periods = (now - deadline).checked_div(interval).unwrap_or(0);
            {
                let tm = &mut self.vtimers[i];
                tm.deadline = if interval > 0 { Some(deadline + (late_periods + 1) * interval) } else { None };
                tm.overrun = tm.overrun.saturating_add(late_periods as i32);
            }
            if let TimerKind::TimerFd { fd, .. } = kind {
                // Make the real descriptor expire now: a one-shot arm of
                // one nanosecond, injected into a stopped thread of the
                // process (it has none stopped only while all its threads
                // are blocked in the kernel; then the next check retries).
                let leader = self.ltids.get(&proc_key).copied();
                let mut tid = self.tasks.values().find(|t| Some(t.tgid) == leader && t.is_ptrace_stopped()).map(|t| t.tid);
                if leader.is_some() && tid.is_none() {
                    // Every thread is blocked in the kernel (typically in
                    // epoll_wait on this very descriptor): interrupt one,
                    // rewind it to re-execute its syscall, and use it.
                    let blocked = self
                        .tasks
                        .values()
                        .find(|t| Some(t.tgid) == leader && matches!(t.state, State::Blocked { .. }) && t.vblock.is_none())
                        .map(|t| t.tid);
                    if let Some(b) = blocked {
                        self.interrupt_blocked(b)?;
                        let t = self.task_mut(b)?;
                        if t.restarting {
                            t.rewind_interrupted_syscall()?;
                            t.restarting = false;
                        } else if matches!(t.stop(), Some(Stop::SyscallExit { ret, .. }) if ret == -(libc::EINTR as i64))
                            && ptrace::interrupting_signals(b) == 0
                        {
                            // epoll waits are not restartable: the interrupt
                            // made this one return EINTR.  Re-execute it
                            // instead of letting the program see that.
                            let nr = t.syscall_nr;
                            let mut r = t.regs()?;
                            r.rip -= ptrace::SYSCALL_INSN_LEN;
                            r.rax = nr;
                            r.orig_rax = nr;
                            t.set_regs(&r)?;
                            t.in_syscall = false;
                            t.rewound = true;
                            t.unreported = false;
                            t.state = State::Stopped(Stop::Parked);
                        }
                        if t.is_ptrace_stopped() {
                            tid = Some(b);
                        }
                    }
                }
                match (leader, tid) {
                    (None, _) => {
                        self.vtimers.remove(i);
                        continue;
                    }
                    (Some(_), None) => {
                        // Undo the re-arm above; try again later.
                        self.vtimers[i].deadline = Some(deadline);
                        i += 1;
                        continue;
                    }
                    (Some(_), Some(tid)) => {
                        let injected: Result<i64> = (|| {
                            let t = self.task_mut(tid)?;
                            let mut remote = crate::remote::Remote::new(t)?;
                            let mut spec = [0u8; 32];
                            spec[24..32].copy_from_slice(&1u64.to_ne_bytes()); // it_value.tv_nsec = 1
                            let addr = remote.push_scratch(&spec)?;
                            let ret = remote.syscall(TIMERFD_SETTIME, &[fd as u64, 0, addr, 0])?;
                            remote.restore()?;
                            Ok(ret)
                        })();
                        let ret = match injected {
                            Ok(ret) => ret,
                            Err(e) if e.is::<crate::remote::TaskDied>() => {
                                // Killed under us: the timer fires later if
                                // another thread of the process is left.
                                self.vtimers[i].deadline = Some(deadline);
                                i += 1;
                                continue;
                            }
                            Err(e) => return Err(e),
                        };
                        if ret < 0 {
                            log::debug!("timerfd {fd}: injected timerfd_settime failed: {ret}");
                        }
                        self.vtimers[i].fires += 1;
                        i += 1;
                        continue;
                    }
                }
            }
            let notify_none = matches!(kind, TimerKind::Posix { notify: SIGEV_NONE, .. });
            let Some(target) = self.signal_target(proc_key, thread, sig) else {
                // The process is gone.
                self.vtimers.remove(i);
                continue;
            };
            if !notify_none {
                if ptrace::pending_signals(target) & sigbit(sig) != 0 {
                    // Standard signals do not queue: this expiry is lost.
                    self.vtimers[i].overrun = self.vtimers[i].overrun.saturating_add(1);
                } else {
                    let mut info = [0u8; 128];
                    info[..4].copy_from_slice(&sig.to_ne_bytes());
                    match kind {
                        TimerKind::Itimer(_) => {
                            info[8..12].copy_from_slice(&SI_KERNEL.to_ne_bytes());
                        }
                        TimerKind::TimerFd { .. } => unreachable!("handled above"),
                        TimerKind::Posix { id, .. } => {
                            info[8..12].copy_from_slice(&SI_TIMER.to_ne_bytes());
                            info[16..20].copy_from_slice(&id.to_ne_bytes());
                            info[20..24].copy_from_slice(&self.vtimers[i].overrun.to_ne_bytes());
                            info[24..32].copy_from_slice(&sigval.to_ne_bytes());
                            self.vtimers[i].overrun = 0;
                        }
                    }
                    let tgid = self.task(target)?.tgid;
                    ptrace::tgkill(tgid, target, sig)?;
                    let ltid = self.task(target)?.ltid;
                    self.siginfo_queue.push((ltid, sig, info));
                    self.vtimers[i].fires += 1;
                }
            }
            i += 1;
        }
        Ok(())
    }

    /// Thread of process `proc_key` that should receive signal `sig`: the
    /// designated thread, else the first (by logical id) live thread that
    /// does not block it, else the first live thread.  Mirrors the kernel's
    /// preference for the main thread.
    pub fn signal_target(&self, proc_key: Ltid, thread: Option<Ltid>, sig: i32) -> Option<pid_t> {
        if let Some(l) = thread {
            let tid = *self.ltids.get(&l)?;
            return self.tasks.get(&tid).filter(|t| t.is_alive()).map(|t| t.tid);
        }
        let leader = *self.ltids.get(&proc_key)?;
        let mut threads: Vec<&Task> = self.tasks.values().filter(|t| t.tgid == leader && t.is_alive()).collect();
        threads.sort_by_key(|t| t.ltid);
        threads
            .iter()
            .find(|t| blocked_signals(t.tid) & sigbit(sig) == 0)
            .or(threads.first())
            .map(|t| t.tid)
    }

    /// Logical id of the thread whose pid, as seen by the tracees, is `pid`.
    pub fn ltid_of_tracee_pid(&self, pid: pid_t) -> Option<Ltid> {
        if self.opts.ns.is_some() {
            let l = pid - crate::ns::CANONICAL_BASE;
            if l >= 1 && self.ltids.contains_key(&(l as Ltid)) {
                return Some(l as Ltid);
            }
            return None;
        }
        self.tasks.get(&pid).map(|t| t.ltid)
    }

    /// Abandon a virtual block: the thread re-executes the syscall when it
    /// next runs (after any pending signal has been delivered).
    pub fn vt_restart(&mut self, tid: pid_t) -> Result<()> {
        let t = self.task_mut(tid)?;
        let Some(vb) = t.vblock.take() else { return Ok(()) };
        let mut r = t.regs()?;
        r.rip -= crate::ptrace::SYSCALL_INSN_LEN;
        r.rax = vb.nr;
        r.orig_rax = vb.nr;
        t.set_regs(&r)?;
        t.in_syscall = false;
        t.rewound = true;
        t.state = State::Stopped(Stop::Parked);
        Ok(())
    }

    /// End a virtual block with result `ret`.
    pub fn vt_wake(&mut self, tid: pid_t, ret: i64) -> Result<()> {
        let now = self.virtual_ns();
        let ltid = self.task(tid)?.ltid;
        if ret == 0 {
            self.note_timer_wake(ltid);
        } else {
            self.note_progress(); // a signal from someone
        }
        let t = self.task_mut(tid)?;
        let Some(vb) = t.vblock.take() else { return Ok(()) };
        if ret == -(libc::EINTR as i64) && vb.rem != 0 {
            let left = vb.deadline.map(|d| d.saturating_sub(now)).unwrap_or(0);
            let _ = write_ts(t, vb.rem, left);
        }
        let regs = t.regs();
        let Ok(mut r) = regs else {
            return self.vanished_or(tid, regs.map(|_| ()));
        };
        r.rax = ret as u64;
        if let Err(e) = t.set_regs(&r) {
            return self.vanished_or(tid, Err(e));
        }
        t.in_syscall = false;
        t.state = State::Stopped(Stop::SyscallExit { nr: vb.nr, ret });
        t.unreported = true;
        Ok(())
    }

    /// `set_regs`/`regs` on a task that died under us (its process was
    /// killed) fail with ESRCH: record the death instead of failing the
    /// run; any other error propagates.
    fn vanished_or(&mut self, tid: pid_t, r: Result<()>) -> Result<()> {
        match r {
            Err(e) if format!("{e:#}").contains("os error 3") => self.task_vanished(tid),
            other => other,
        }
    }

    /// Give a thread blocked in the kernel with a stripped timeout its
    /// timeout result.
    fn vt_timeout(&mut self, tid: pid_t) -> Result<()> {
        let Some((_, ret)) = self.task(tid)?.vdeadline else { return Ok(()) };
        let ltid = self.task(tid)?.ltid;
        self.note_timer_wake(ltid);
        self.interrupt_blocked(tid)?;
        let t = self.task_mut(tid)?;
        // epoll waits are not restartable: PTRACE_INTERRUPT makes them
        // return EINTR instead of an internal ERESTART result.
        let interrupted = matches!(t.stop(), Some(Stop::SyscallExit { ret, .. })
            if ret == -(libc::EINTR as i64));
        if !t.restarting && !interrupted {
            // It completed for real just before (left unreported), or died.
            return Ok(());
        }
        let nr = t.syscall_nr;
        let regs = t.regs();
        let Ok(mut r) = regs else {
            return self.vanished_or(tid, regs.map(|_| ()));
        };
        r.rax = ret as u64;
        if let Err(e) = t.set_regs(&r) {
            return self.vanished_or(tid, Err(e));
        }
        t.restarting = false;
        t.in_syscall = false;
        t.state = State::Stopped(Stop::SyscallExit { nr, ret });
        t.unreported = true;
        self.vt_syscall_done(tid)?;
        Ok(())
    }

    /// Wake `tid` if its virtual block or deadline is over (or a signal is
    /// pending for a virtually blocked thread).
    pub fn vt_poll_task(&mut self, tid: pid_t) -> Result<()> {
        let now = self.virtual_ns();
        let t = self.task(tid)?;
        if t.vwait.is_some() {
            return self.proc_poll_wait(tid);
        }
        if let Some(vb) = t.vblock {
            if vb.deadline.map(|d| d <= now).unwrap_or(false) {
                return self.vt_wake(tid, 0);
            }
            // Only a signal the program would actually be interrupted by
            // (not ignored, not the tracer's own preemption signal); the
            // pending set (/proc) is re-read only after a signal was sent.
            let epoch = ptrace::sig_epoch();
            if t.sig_epoch_seen != epoch {
                self.task_mut(tid)?.sig_epoch_seen = epoch;
                if ptrace::interrupting_signals(tid) != 0 {
                    return self.vt_wake(tid, -(libc::EINTR as i64));
                }
            }
        } else if matches!(t.state, State::Blocked { .. }) {
            if let Some((d, _)) = t.vdeadline {
                if d <= now {
                    return self.vt_timeout(tid);
                }
            }
        }
        Ok(())
    }

    /// Fire timers and wake threads whose virtual deadlines have passed.
    pub fn vt_check(&mut self) -> Result<()> {
        self.proc_check_exits()?;
        if self.opts.virtual_time {
            self.vt_fire_timers()?;
        }
        let tids: Vec<pid_t> = self
            .tasks
            .values()
            .filter(|t| t.is_alive() && (t.vblock.is_some() || t.vdeadline.is_some()))
            .map(|t| t.tid)
            .collect();
        for tid in tids {
            self.vt_poll_task(tid)?;
        }
        Ok(())
    }

    /// Earliest pending virtual deadline (sleeps, timeouts, timers).
    pub fn vt_next_deadline(&self) -> Option<u64> {
        let mut best: Option<u64> = None;
        let mut consider = |d: Option<u64>| {
            if let Some(d) = d {
                best = Some(best.map_or(d, |b: u64| b.min(d)));
            }
        };
        for t in self.tasks.values().filter(|t| t.is_alive()) {
            if let Some(vb) = t.vblock {
                consider(vb.deadline);
            } else if matches!(t.state, State::Blocked { .. }) {
                consider(t.vdeadline.map(|d| d.0));
            }
        }
        for tm in &self.vtimers {
            if !matches!(tm.kind, TimerKind::Posix { notify: SIGEV_NONE, .. }) {
                consider(tm.deadline);
            }
        }
        best
    }

    /// Nothing can run: jump the virtual clock to the next deadline and act
    /// on it.  Returns false if there is no deadline to jump to.
    pub fn vt_advance_idle(&mut self) -> Result<bool> {
        if !self.opts.virtual_time {
            return Ok(false);
        }
        let Some(d) = self.vt_next_deadline() else { return Ok(false) };
        let now = self.virtual_ns();
        if d > now {
            self.vclock.skew += d - now;
        }
        self.vt_check()?;
        Ok(true)
    }

    /// The event for a syscall-exit stop of a syscall that was emulated.
    pub fn vt_emulated_exit(&mut self, tid: pid_t, nr: u64, em: Emulation) -> Result<Event> {
        match em {
            Emulation::Ret(v) => {
                let t = self.task_mut(tid)?;
                let mut r = t.regs()?;
                r.rax = v as u64;
                t.set_regs(&r)?;
                Ok(Event::SyscallExit { nr, ret: v, child: None })
            }
            Emulation::Block(vb) => {
                let t = self.task_mut(tid)?;
                t.vblock = Some(vb);
                t.state = State::Blocked { nr };
                self.vt_poll_task(tid)?;
                let t = self.task_mut(tid)?;
                if t.unreported {
                    t.unreported = false;
                    if let State::Stopped(Stop::SyscallExit { nr, ret }) = t.state {
                        return Ok(Event::SyscallExit { nr, ret, child: None });
                    }
                }
                Ok(Event::Blocked { nr })
            }
        }
    }
}
