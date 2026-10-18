//! Thin wrappers around the raw ptrace(2) / waitpid(2) interface.
//!
//! We deliberately avoid the `nix` crate's ptrace module here: the set of
//! requests we need (regsets, sigmask, event messages, seize/interrupt) is
//! small and stable, and mapping them directly onto libc keeps the semantics
//! obvious when reading alongside rr's `Task.cc`.

use anyhow::{bail, anyhow, Context, Result};
use libc::{c_long, c_void, pid_t, user_regs_struct};
use std::io;
use std::mem::MaybeUninit;

pub type Regs = user_regs_struct;

/// Options we set on every tracee (mirrors `Task::ptrace_seize` in rr).
pub const SEIZE_OPTIONS: c_long = (libc::PTRACE_O_TRACESYSGOOD
    | libc::PTRACE_O_TRACECLONE
    | libc::PTRACE_O_TRACEFORK
    | libc::PTRACE_O_TRACEVFORK
    | libc::PTRACE_O_TRACEEXEC
    | libc::PTRACE_O_TRACEEXIT
    | libc::PTRACE_O_EXITKILL) as c_long;

/// Signal used by the perf counter overflow (same choice as rr).
pub const TIME_SLICE_SIGNAL: i32 = libc::SIGSTKFLT;

pub const PTRACE_EVENT_STOP: i32 = 128;

const NT_X86_XSTATE: c_long = 0x202;

fn errno() -> io::Error {
    io::Error::last_os_error()
}

unsafe fn raw(req: u32, pid: pid_t, addr: *mut c_void, data: *mut c_void) -> Result<c_long> {
    *libc::__errno_location() = 0;
    let r = libc::ptrace(req, pid, addr, data);
    if r == -1 && *libc::__errno_location() != 0 {
        return Err(anyhow!("ptrace({req}, {pid}) failed: {}", errno()));
    }
    Ok(r)
}

pub fn seize(pid: pid_t, options: c_long) -> Result<()> {
    unsafe {
        raw(
            libc::PTRACE_SEIZE,
            pid,
            std::ptr::null_mut(),
            options as *mut c_void,
        )
    }
    .with_context(|| format!("PTRACE_SEIZE {pid}"))?;
    Ok(())
}

pub fn interrupt(pid: pid_t) -> Result<()> {
    unsafe {
        raw(
            libc::PTRACE_INTERRUPT,
            pid,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    }?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeHow {
    Cont,
    Syscall,
    SingleStep,
}

pub fn resume(pid: pid_t, how: ResumeHow, sig: i32) -> Result<()> {
    let req = match how {
        ResumeHow::Cont => libc::PTRACE_CONT,
        ResumeHow::Syscall => libc::PTRACE_SYSCALL,
        ResumeHow::SingleStep => libc::PTRACE_SINGLESTEP,
    };
    unsafe { raw(req, pid, std::ptr::null_mut(), sig as c_long as *mut c_void) }
        .with_context(|| format!("resume {pid} {how:?} sig={sig}"))?;
    Ok(())
}

/// Output descriptions and virtualization use the native x86-64 syscall ABI.
/// Reject compat entry before interpreting its overlapping syscall numbers.
pub fn check_syscall_abi(pid: pid_t, nr: u64) -> Result<()> {
    let mut info: libc::ptrace_syscall_info = unsafe { std::mem::zeroed() };
    let len = unsafe {
        raw(libc::PTRACE_GET_SYSCALL_INFO, pid,
            std::mem::size_of_val(&info) as *mut c_void,
            &mut info as *mut _ as *mut c_void)?
    };
    const AUDIT_ARCH_X86_64: u32 = 0xc000003e;
    const X32_SYSCALL_BIT: u64 = 0x40000000;
    if len < 24 || info.arch != AUDIT_ARCH_X86_64 || (nr <= u32::MAX as u64 && nr & X32_SYSCALL_BIT != 0) {
        bail!("unsupported syscall ABI: arch {:#x}, syscall {nr}; ffs requires native x86-64", info.arch);
    }
    Ok(())
}

pub fn get_regs(pid: pid_t) -> Result<Regs> {
    let mut regs = MaybeUninit::<Regs>::uninit();
    unsafe {
        raw(
            libc::PTRACE_GETREGS,
            pid,
            std::ptr::null_mut(),
            regs.as_mut_ptr() as *mut c_void,
        )
        .with_context(|| format!("PTRACE_GETREGS {pid}"))?;
        Ok(regs.assume_init())
    }
}

pub fn set_regs(pid: pid_t, regs: &Regs) -> Result<()> {
    unsafe {
        raw(
            libc::PTRACE_SETREGS,
            pid,
            std::ptr::null_mut(),
            regs as *const Regs as *mut c_void,
        )
        .with_context(|| format!("PTRACE_SETREGS {pid}"))?;
    }
    Ok(())
}

/// Read the full XSAVE area (FP/SSE/AVX state).
pub fn get_xstate(pid: pid_t) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; 16384];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut c_void,
        iov_len: buf.len(),
    };
    unsafe {
        raw(
            libc::PTRACE_GETREGSET,
            pid,
            NT_X86_XSTATE as *mut c_void,
            &mut iov as *mut _ as *mut c_void,
        )
        .with_context(|| format!("PTRACE_GETREGSET XSTATE {pid}"))?;
    }
    buf.truncate(iov.iov_len);
    Ok(buf)
}

pub fn set_xstate(pid: pid_t, data: &[u8]) -> Result<()> {
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut c_void,
        iov_len: data.len(),
    };
    unsafe {
        raw(
            libc::PTRACE_SETREGSET,
            pid,
            NT_X86_XSTATE as *mut c_void,
            &mut iov as *mut _ as *mut c_void,
        )
        .with_context(|| format!("PTRACE_SETREGSET XSTATE {pid}"))?;
    }
    Ok(())
}

pub fn get_sigmask(pid: pid_t) -> Result<u64> {
    let mut mask: u64 = 0;
    unsafe {
        raw(
            libc::PTRACE_GETSIGMASK,
            pid,
            8 as *mut c_void,
            &mut mask as *mut u64 as *mut c_void,
        )
        .with_context(|| format!("PTRACE_GETSIGMASK {pid}"))?;
    }
    Ok(mask)
}

pub fn set_sigmask(pid: pid_t, mask: u64) -> Result<()> {
    let mut m = mask;
    unsafe {
        raw(
            libc::PTRACE_SETSIGMASK,
            pid,
            8 as *mut c_void,
            &mut m as *mut u64 as *mut c_void,
        )
        .with_context(|| format!("PTRACE_SETSIGMASK {pid}"))?;
    }
    Ok(())
}

/// Raw `siginfo_t` (128 bytes) of the signal a stopped task is about to get.
pub fn get_siginfo(pid: pid_t) -> Result<[u8; 128]> {
    let mut info = [0u8; 128];
    unsafe {
        raw(
            libc::PTRACE_GETSIGINFO,
            pid,
            std::ptr::null_mut(),
            info.as_mut_ptr() as *mut c_void,
        )
        .with_context(|| format!("PTRACE_GETSIGINFO {pid}"))?;
    }
    Ok(info)
}

pub fn set_siginfo(pid: pid_t, info: &[u8; 128]) -> Result<()> {
    let mut i = *info;
    unsafe {
        raw(
            libc::PTRACE_SETSIGINFO,
            pid,
            std::ptr::null_mut(),
            i.as_mut_ptr() as *mut c_void,
        )
        .with_context(|| format!("PTRACE_SETSIGINFO {pid}"))?;
    }
    Ok(())
}

pub fn siginfo_to_hex(info: &[u8; 128]) -> String {
    info.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn siginfo_from_hex(s: &str) -> Option<[u8; 128]> {
    if s.len() != 256 {
        return None;
    }
    let mut out = [0u8; 128];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// `si_code` of a raw siginfo.
pub fn siginfo_code(info: &[u8; 128]) -> i32 {
    i32::from_ne_bytes(info[8..12].try_into().unwrap())
}

/// Signals pending for a thread (thread-directed and process-directed), as
/// a bit mask (bit `n-1` for signal `n`).
pub fn pending_signals(tid: pid_t) -> u64 {
    let Ok(s) = std::fs::read_to_string(format!("/proc/{tid}/status")) else {
        return 0;
    };
    let mut mask = 0u64;
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("SigPnd:").or_else(|| line.strip_prefix("ShdPnd:")) {
            mask |= u64::from_str_radix(v.trim(), 16).unwrap_or(0);
        }
    }
    mask
}

fn status_mask(tid: pid_t, field: &str) -> u64 {
    let Ok(s) = std::fs::read_to_string(format!("/proc/{tid}/status")) else {
        return 0;
    };
    s.lines()
        .find_map(|l| l.strip_prefix(field))
        .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
        .unwrap_or(0)
}

/// Signals pending for the thread itself (not process-directed ones).
pub fn thread_pending_signals(tid: pid_t) -> u64 {
    status_mask(tid, "SigPnd:")
}

/// Process-directed signals pending for `tid`'s process.
pub fn shared_pending_signals(tid: pid_t) -> u64 {
    status_mask(tid, "ShdPnd:")
}

/// Signals a thread currently blocks.
pub fn blocked_signals(tid: pid_t) -> u64 {
    status_mask(tid, "SigBlk:")
}

/// Signals the process of `tid` ignores.
pub fn ignored_signals(tid: pid_t) -> u64 {
    status_mask(tid, "SigIgn:")
}

/// Signals the process of `tid` has a handler for.
pub fn caught_signals(tid: pid_t) -> u64 {
    status_mask(tid, "SigCgt:")
}

/// Pending, unblocked signals of `tid` that would interrupt a sleep or a
/// blocking syscall if the thread were not traced: those with a handler,
/// or with a default action that is not "ignore".  (Under ptrace the
/// kernel queues ignored signals too, for the tracer's benefit.)
pub fn interrupting_signals(tid: pid_t) -> u64 {
    const DEFAULT_IGNORED: u64 = (1 << (libc::SIGCHLD - 1))
        | (1 << (libc::SIGURG - 1))
        | (1 << (libc::SIGWINCH - 1))
        | (1 << (libc::SIGCONT - 1));
    let pending = pending_signals(tid) & !blocked_signals(tid) & !(1u64 << (TIME_SLICE_SIGNAL - 1));
    let caught = caught_signals(tid);
    let ignored = ignored_signals(tid);
    pending & (caught | !(ignored | DEFAULT_IGNORED))
}

pub fn tgkill(tgid: pid_t, tid: pid_t, sig: i32) -> Result<()> {
    note_signal_sent();
    if unsafe { libc::syscall(libc::SYS_tgkill, tgid, tid, sig) } < 0 {
        bail!("tgkill({tgid}, {tid}, {sig}): {}", std::io::Error::last_os_error());
    }
    Ok(())
}

pub fn get_event_msg(pid: pid_t) -> Result<u64> {
    let mut msg: libc::c_ulong = 0;
    unsafe {
        raw(
            libc::PTRACE_GETEVENTMSG,
            pid,
            std::ptr::null_mut(),
            &mut msg as *mut _ as *mut c_void,
        )?;
    }
    Ok(msg as u64)
}

pub fn peek(pid: pid_t, addr: usize) -> Result<u64> {
    let v = unsafe {
        raw(
            libc::PTRACE_PEEKDATA,
            pid,
            addr as *mut c_void,
            std::ptr::null_mut(),
        )
    }?;
    Ok(v as u64)
}

pub fn poke(pid: pid_t, addr: usize, val: u64) -> Result<()> {
    unsafe {
        raw(
            libc::PTRACE_POKEDATA,
            pid,
            addr as *mut c_void,
            val as *mut c_void,
        )
    }?;
    Ok(())
}

/// Decoded wait status of a tracee.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitStatus {
    /// Normal exit with code.
    Exited(i32),
    /// Killed by signal.
    Signaled(i32),
    /// Syscall entry or exit stop (TRACESYSGOOD).
    SyscallStop,
    /// `PTRACE_EVENT_*` stop (event number, e.g. PTRACE_EVENT_CLONE).
    Event(i32),
    /// Signal-delivery-stop for `sig`.
    Signal(i32),
    /// Group-stop (seize semantics: PTRACE_EVENT_STOP with a stop signal).
    GroupStop(i32),
}

impl WaitStatus {
    pub fn decode(status: i32) -> WaitStatus {
        if libc::WIFEXITED(status) {
            return WaitStatus::Exited(libc::WEXITSTATUS(status));
        }
        if libc::WIFSIGNALED(status) {
            return WaitStatus::Signaled(libc::WTERMSIG(status));
        }
        debug_assert!(libc::WIFSTOPPED(status));
        let sig = libc::WSTOPSIG(status);
        let event = status >> 16;
        if event != 0 {
            if event == PTRACE_EVENT_STOP {
                if sig == libc::SIGTRAP {
                    // PTRACE_INTERRUPT or initial seize stop
                    return WaitStatus::Event(PTRACE_EVENT_STOP);
                }
                return WaitStatus::GroupStop(sig);
            }
            return WaitStatus::Event(event);
        }
        if sig == libc::SIGTRAP | 0x80 {
            return WaitStatus::SyscallStop;
        }
        WaitStatus::Signal(sig)
    }
}

/// Blocking wait for a specific tid.  Returns None if the tid no longer exists.
pub fn wait_tid(tid: pid_t) -> Result<Option<WaitStatus>> {
    wait_impl(tid, 0)
}

/// Non-blocking wait for a specific tid.
pub fn try_wait_tid(tid: pid_t) -> Result<Option<WaitStatus>> {
    wait_impl(tid, libc::WNOHANG)
}

thread_local! {
    /// Wait statuses collected by `drain_statuses` (one `waitpid(-1)` sweep
    /// instead of one `waitpid` per thread) and not yet handed out.  Every
    /// wait goes through `wait_impl`, which looks here first, so a status
    /// swept up for a thread someone else is waiting for is not lost.
    static STASH: std::cell::RefCell<std::collections::HashMap<pid_t, std::collections::VecDeque<WaitStatus>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Collect every wait status currently available for any child or tracee.
pub fn drain_statuses() -> Result<()> {
    loop {
        let mut status: i32 = 0;
        let r = unsafe { libc::waitpid(-1, &mut status, libc::__WALL | libc::WNOHANG) };
        if r == -1 {
            let e = errno();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            if e.raw_os_error() == Some(libc::ECHILD) {
                return Ok(());
            }
            return Err(anyhow!("waitpid(-1) failed: {e}"));
        }
        if r == 0 {
            return Ok(());
        }
        STASH.with(|st| st.borrow_mut().entry(r).or_default().push_back(WaitStatus::decode(status)));
    }
}

/// Put a status back for a later wait.
pub fn stash(tid: pid_t, st: WaitStatus) {
    STASH.with(|s| s.borrow_mut().entry(tid).or_default().push_front(st));
}

/// A status `drain_statuses` collected for `tid`, if any (no syscall).
pub fn take_stashed(tid: pid_t) -> Option<WaitStatus> {
    STASH.with(|st| {
        let mut st = st.borrow_mut();
        let q = st.get_mut(&tid)?;
        let v = q.pop_front();
        if q.is_empty() {
            st.remove(&tid);
        }
        v
    })
}

/// Signals sent to tracees since the start (by tracees or by the tracer):
/// a parked sleeper only needs its pending set re-read when this changed.
pub static SIG_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn note_signal_sent() {
    SIG_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

pub fn sig_epoch() -> u64 {
    SIG_EPOCH.load(std::sync::atomic::Ordering::Relaxed)
}

fn wait_impl(tid: pid_t, extra: i32) -> Result<Option<WaitStatus>> {
    if let Some(st) = take_stashed(tid) {
        return Ok(Some(st));
    }
    loop {
        let mut status: i32 = 0;
        let r = unsafe { libc::waitpid(tid, &mut status, libc::__WALL | extra) };
        if r == -1 {
            let e = errno();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            if e.raw_os_error() == Some(libc::ECHILD) {
                return Ok(None);
            }
            return Err(anyhow!("waitpid({tid}) failed: {e}"));
        }
        if r == 0 {
            return Ok(None);
        }
        return Ok(Some(WaitStatus::decode(status)));
    }
}

/// Length in bytes of the x86-64 `syscall` instruction.
pub const SYSCALL_INSN_LEN: u64 = 2;
pub const SYSCALL_INSN: [u8; 2] = [0x0f, 0x05];
pub const RDTSC_INSN: [u8; 2] = [0x0f, 0x31];
pub const RDTSCP_INSN: [u8; 3] = [0x0f, 0x01, 0xf9];

/// Restartable syscall results (negative errno values the kernel uses
/// internally when a syscall is interrupted by a stop).
pub const ERESTARTSYS: i64 = -512;
pub const ERESTARTNOINTR: i64 = -513;
pub const ERESTARTNOHAND: i64 = -514;
pub const ERESTART_RESTARTBLOCK: i64 = -516;

pub fn is_restart_result(rax: u64) -> bool {
    let v = rax as i64;
    v == ERESTARTSYS || v == ERESTARTNOINTR || v == ERESTARTNOHAND || v == ERESTART_RESTARTBLOCK
}

/// Read the scheduler state letter ('R','S','D','t','T','Z','X', ...) of a thread.
pub fn proc_state(tid: pid_t) -> Option<char> {
    let s = std::fs::read_to_string(format!("/proc/{tid}/stat")).ok()?;
    // state follows the ") " that closes the comm field
    let idx = s.rfind(") ")?;
    s[idx + 2..].chars().next()
}

/// Wait up to `timeout` for a SIGCHLD (which must be blocked in this
/// thread; `Session` blocks it).  A pending one is consumed.  Returns
/// whether one arrived.
pub fn wait_sigchld(timeout: std::time::Duration) -> bool {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGCHLD);
        let ts = libc::timespec {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_nsec: timeout.subsec_nanos() as libc::c_long,
        };
        libc::sigtimedwait(&set, std::ptr::null_mut(), &ts) > 0
    }
}
