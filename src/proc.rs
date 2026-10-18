//! Emulated process tree.  Every process a tracee creates is made a child
//! of ffs itself (`CLONE_PARENT` is added to `fork`/`vfork`/`clone`/
//! `clone3`), and the parent/child relationships the program expects are
//! kept by the tracer in terms of logical thread ids, which a checkpoint
//! copies unchanged.  `wait4`, `waitid`, `getppid` and the `SIGCHLD` of an
//! exiting child are emulated from that tree, so a process forked before a
//! checkpoint is still the child of its parent in every branch (the kernel
//! would have made the copies children of ffs).  This mirrors how rr
//! replays these syscalls from its recording instead of executing them.
//!
//! Exit statuses are known to the tracer because it reaps every tracee.

use anyhow::Result;
use libc::pid_t;
use std::time::{Duration, Instant};

use crate::ptrace;
use crate::remote::{self, Remote};
use crate::session::Session;
use crate::task::Ltid;
use crate::vtime::{Emulation, VBlock};

const CLONE: u64 = 56;
const FORK: u64 = 57;
const VFORK: u64 = 58;
const WAIT4: u64 = 61;
const KILL: u64 = 62;
const EVENTFD2: u64 = 290;
const PIDFD_OPEN: u64 = 434;
const GETPPID: u64 = 110;
const WAITID: u64 = 247;
const CLONE3: u64 = 435;
const RT_SIGACTION: u64 = 13;

const CLONE_VM: u64 = 0x100;
const CLONE_VFORK: u64 = 0x4000;
const CLONE_PARENT: u64 = 0x8000;
const CLONE_THREAD: u64 = 0x10000;

const WNOHANG: u64 = 1;
const WUNTRACED: u64 = 2; // also WSTOPPED
const WEXITED: u64 = 4;
const WCONTINUED: u64 = 8;
const WNOWAIT: u64 = 0x0100_0000;
const P_ALL: u64 = 0;
const P_PID: u64 = 1;
const P_PGID: u64 = 2;

const CLD_EXITED: i32 = 1;
const CLD_KILLED: i32 = 2;
const CLD_DUMPED: i32 = 3;
const CLD_STOPPED: i32 = 5;
const CLD_CONTINUED: i32 = 6;
const SA_RESTART: u64 = 0x1000_0000;

/// A child that has exited and has not been waited for yet.
#[derive(Clone, Debug)]
pub struct VZombie {
    pub key: Ltid,
    pub parent: Ltid,
    /// Raw wait status (`exit code << 8`, or the signal number).
    pub status: i32,
    /// Process group (host view) at exit, for `wait4(-pgid)`.
    pub pgid: pid_t,
}

/// A child that stopped (job control) or continued and has not been
/// reported by a wait yet.  Tracees never really stop: a process whose
/// stop signal has the default action is simply not scheduled until a
/// `SIGCONT` (or `SIGKILL`) is pending for it, the way rr emulates group
/// stops.
#[derive(Clone, Debug)]
pub struct VJobStop {
    pub key: Ltid,
    pub parent: Ltid,
    /// The stop signal, or `SIGCONT`.
    pub sig: i32,
    pub pgid: pid_t,
}

#[derive(Clone, Copy, Debug)]
pub enum WaitSel {
    Any,
    Pid(Ltid),
    /// Host process group id.
    Pgid(pid_t),
}

/// An emulated, blocking wait in progress.
#[derive(Clone, Copy, Debug)]
pub struct VWait {
    pub nr: u64,
    pub sel: WaitSel,
    pub options: u64,
    /// `wait4`: `int *status`; `waitid`: `siginfo_t *infop`.
    pub out: u64,
    pub rusage: u64,
}

enum WaitOutcome {
    Reap(usize),
    /// Index into `jobstops` / `jobconts`.
    Stopped(usize),
    Continued(usize),
    Nothing,
    NoChildren,
}

pub fn host_pgid(pid: pid_t) -> Option<pid_t> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &s[s.rfind(") ")? + 2..];
    rest.split_whitespace().nth(2)?.parse().ok()
}

impl Session {
    /// The pid of process `key` as the tracees see it.
    pub fn tracee_pid(&self, key: Ltid) -> pid_t {
        if self.opts.ns.is_some() {
            crate::ns::canonical_tid(key)
        } else {
            self.ltids.get(&key).copied().unwrap_or(0)
        }
    }

    fn proc_alive(&self, key: Ltid) -> bool {
        let Some(&leader) = self.ltids.get(&key) else { return false };
        self.tasks.values().any(|t| t.tgid == leader && t.is_alive())
    }

    /// Intercept process-tree syscalls at their entry stop.
    pub fn proc_pre_syscall(&mut self, tid: pid_t, nr: u64) -> Result<()> {
        let key = self.proc_key(tid)?;
        let t = self.task_mut(tid)?;
        let r = t.regs()?;
        // A process that has just unshared a pid namespace creates its init
        // with the next fork; the kernel refuses CLONE_PARENT there, so that
        // child really becomes the tracee's child (ffs still traces and
        // reaps it as its tracer).
        let new_ns_pending = {
            let a = std::fs::read_link(format!("/proc/{tid}/ns/pid")).ok();
            let b = std::fs::read_link(format!("/proc/{tid}/ns/pid_for_children")).ok();
            a.is_some() && a != b
        };
        match nr {
            CLONE | FORK | VFORK if new_ns_pending => {}
            CLONE3 if new_ns_pending => {}
            CLONE | FORK | VFORK => {
                let mut n = r;
                match nr {
                    CLONE => {
                        if r.rdi & CLONE_THREAD != 0 || r.rdi & CLONE_PARENT != 0 {
                            return Ok(());
                        }
                        n.rdi |= CLONE_PARENT;
                    }
                    FORK => {
                        n.orig_rax = CLONE;
                        n.rdi = libc::SIGCHLD as u64 | CLONE_PARENT;
                        n.rsi = 0;
                        n.rdx = 0;
                        n.r10 = 0;
                        n.r8 = 0;
                    }
                    _ => {
                        n.orig_rax = CLONE;
                        n.rdi = CLONE_VFORK | CLONE_VM | libc::SIGCHLD as u64 | CLONE_PARENT;
                        n.rsi = 0;
                        n.rdx = 0;
                        n.r10 = 0;
                        n.r8 = 0;
                    }
                }
                t.set_regs(&n)?;
                if t.entry_regs.is_none() {
                    t.entry_regs = Some(r);
                }
            }
            CLONE3 => {
                if r.rdi == 0 || r.rsi < 8 {
                    return Ok(());
                }
                let flags = t.mem.read_u64(r.rdi)?;
                if flags & (CLONE_THREAD | CLONE_PARENT) == 0 {
                    t.mem.write_u64(r.rdi, flags | CLONE_PARENT)?;
                    t.mem_restore = Some((r.rdi, flags.to_ne_bytes().to_vec()));
                }
            }
            GETPPID => {
                let ppid = match self.parent_of.get(&key) {
                    Some(&p) if self.proc_alive(p) => self.tracee_pid(p),
                    _ => 1,
                };
                self.emulate_syscall(tid, Emulation::Ret(ppid as i64))?;
            }
            KILL => {
                // A child the tracer has reaped but the tracee has not: the
                // kernel would still accept a signal to the zombie (and do
                // nothing), while the real pid is gone.
                let pid = r.rdi as i32;
                if pid > 0 {
                    if let Some(l) = self.ltid_of_tracee_pid(pid) {
                        if self.vzombies.iter().any(|z| z.key == l) {
                            let sig = r.rsi as i32;
                            let ret = if (0..=64).contains(&sig) { 0 } else { -(libc::EINVAL as i64) };
                            self.emulate_syscall(tid, Emulation::Ret(ret))?;
                        }
                    }
                }
            }
            PIDFD_OPEN => {
                // Same situation: a pidfd of a reaped child cannot be made.
                // Stand in an eventfd that is readable at once, which is
                // what polling the pidfd of a zombie reports (tokio's
                // process reaper polls, then calls waitpid).
                let pid = r.rdi as i32;
                if let Some(l) = self.ltid_of_tracee_pid(pid) {
                    self.catch_up_exits(&[l])?;
                    if self.vzombies.iter().any(|z| z.key == l) {
                        let t = self.task_mut(tid)?;
                        let mut n = r;
                        n.orig_rax = EVENTFD2;
                        n.rdi = 1;
                        n.rsi = libc::EFD_CLOEXEC as u64 | (r.rsi & libc::O_NONBLOCK as u64);
                        t.set_regs(&n)?;
                        if t.entry_regs.is_none() {
                            t.entry_regs = Some(r);
                        }
                    }
                }
            }
            WAIT4 | WAITID => {
                let children: Vec<Ltid> = self.parent_of.iter().filter(|(_, p)| **p == key).map(|(c, _)| *c).collect();
                self.catch_up_exits(&children)?;
                let r = self.task_mut(tid)?.regs()?;
                let vw = match nr {
                    WAIT4 => {
                        let pid = r.rdi as i32;
                        let sel = match pid {
                            -1 => WaitSel::Any,
                            0 => WaitSel::Pgid(host_pgid(tid).unwrap_or(tid)),
                            p if p > 0 => match self.ltid_of_tracee_pid(p) {
                                Some(l) => WaitSel::Pid(l),
                                None => WaitSel::Pid(0),
                            },
                            p => match self.ltid_of_tracee_pid(-p) {
                                Some(l) => WaitSel::Pgid(self.ltids.get(&l).copied().unwrap_or(0)),
                                None => WaitSel::Pgid(0),
                            },
                        };
                        VWait { nr, sel, options: r.rdx, out: r.rsi, rusage: r.r10 }
                    }
                    _ => {
                        let sel = match r.rdi {
                            P_ALL => WaitSel::Any,
                            P_PID => match self.ltid_of_tracee_pid(r.rsi as i32) {
                                Some(l) => WaitSel::Pid(l),
                                None => WaitSel::Pid(0),
                            },
                            P_PGID => {
                                if r.rsi == 0 {
                                    WaitSel::Pgid(host_pgid(tid).unwrap_or(tid))
                                } else {
                                    match self.ltid_of_tracee_pid(r.rsi as i32) {
                                        Some(l) => WaitSel::Pgid(self.ltids.get(&l).copied().unwrap_or(0)),
                                        None => WaitSel::Pgid(0),
                                    }
                                }
                            }
                            _ => {
                                self.emulate_syscall(tid, Emulation::Ret(-(libc::ECHILD as i64)))?;
                                return Ok(());
                            }
                        };
                        if r.r10 & WEXITED == 0 {
                            // Only exits are ever reported (tracees never
                            // stop); waiting for anything else blocks forever
                            // unless there are no children at all.
                        }
                        VWait { nr, sel, options: r.r10, out: r.rdx, rusage: r.r8 }
                    }
                };
                match self.eval_wait(key, &vw) {
                    WaitOutcome::Reap(i) => {
                        let ret = self.reap_zombie(tid, &vw, i)?;
                        self.emulate_syscall(tid, Emulation::Ret(ret))?;
                    }
                    WaitOutcome::Stopped(i) => {
                        let ret = self.report_jobstop(tid, &vw, i, false)?;
                        self.emulate_syscall(tid, Emulation::Ret(ret))?;
                    }
                    WaitOutcome::Continued(i) => {
                        let ret = self.report_jobstop(tid, &vw, i, true)?;
                        self.emulate_syscall(tid, Emulation::Ret(ret))?;
                    }
                    WaitOutcome::NoChildren => {
                        self.emulate_syscall(tid, Emulation::Ret(-(libc::ECHILD as i64)))?;
                    }
                    WaitOutcome::Nothing if vw.options & WNOHANG != 0 => {
                        if nr == WAITID && vw.out != 0 {
                            // Linux clears si_pid and si_signo here.
                            let t = self.task_mut(tid)?;
                            t.mem.write(vw.out, &[0u8; 4])?;
                            t.mem.write(vw.out + 16, &[0u8; 4])?;
                        }
                        self.emulate_syscall(tid, Emulation::Ret(0))?;
                    }
                    WaitOutcome::Nothing => {
                        self.task_mut(tid)?.vwait = Some(vw);
                        self.emulate_syscall(tid, Emulation::Block(VBlock { nr, deadline: None, rem: 0 }))?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// A process past its exit event is dead or dying in the kernel: its
    /// pidfd polls readable and a real `waitpid` would find it, while the
    /// tracer may not have collected its status yet.  Catch up before
    /// answering a wait or a `pidfd_open`, so that the emulated process
    /// tree agrees with what the kernel has shown the tracee.
    fn catch_up_exits(&mut self, procs: &[Ltid]) -> Result<()> {
        ptrace::drain_statuses()?;
        self.reap_zombies();
        let dying: Vec<pid_t> = procs
            .iter()
            .filter(|k| !self.exited_procs.contains(k))
            .filter_map(|k| self.ltids.get(k).copied())
            .filter(|leader| self.zombies.contains(leader) && !self.tasks.values().any(|t| t.tgid == *leader && t.is_alive()))
            .collect();
        if !dying.is_empty() {
            let start = Instant::now();
            while dying.iter().any(|l| self.zombies.contains(l)) && start.elapsed() < Duration::from_millis(500) {
                std::thread::sleep(Duration::from_micros(50));
                ptrace::drain_statuses()?;
                self.reap_zombies();
            }
        }
        self.proc_check_exits()
    }

    fn child_matches(&self, child: Ltid, sel: &WaitSel, zombie_pgid: Option<pid_t>) -> bool {
        match *sel {
            WaitSel::Any => true,
            WaitSel::Pid(l) => l == child,
            WaitSel::Pgid(pg) => {
                let pgid = zombie_pgid.or_else(|| self.ltids.get(&child).and_then(|&p| host_pgid(p)));
                pgid == Some(pg)
            }
        }
    }

    fn eval_wait(&self, key: Ltid, vw: &VWait) -> WaitOutcome {
        let mut any = false;
        // Zombies are reported oldest first, like the kernel's children list.
        let mut best: Option<(Ltid, usize)> = None;
        for (i, z) in self.vzombies.iter().enumerate() {
            if z.parent == key && self.child_matches(z.key, &vw.sel, Some(z.pgid)) {
                any = true;
                if (vw.nr == WAIT4 || vw.options & WEXITED != 0) && best.map(|(k, _)| z.key < k).unwrap_or(true) {
                    best = Some((z.key, i));
                }
            }
        }
        if let Some((_, i)) = best {
            return WaitOutcome::Reap(i);
        }
        if vw.options & WUNTRACED != 0 {
            for (i, j) in self.jobstops.iter().enumerate() {
                if j.parent == key && self.child_matches(j.key, &vw.sel, Some(j.pgid)) {
                    return WaitOutcome::Stopped(i);
                }
            }
        }
        if vw.options & WCONTINUED != 0 {
            for (i, j) in self.jobconts.iter().enumerate() {
                if j.parent == key && self.child_matches(j.key, &vw.sel, Some(j.pgid)) {
                    return WaitOutcome::Continued(i);
                }
            }
        }
        for (&c, &p) in &self.parent_of {
            if p == key && self.proc_alive(c) && self.child_matches(c, &vw.sel, None) {
                any = true;
            }
        }
        if any {
            WaitOutcome::Nothing
        } else {
            WaitOutcome::NoChildren
        }
    }

    /// Write the results of a successful wait for zombie `i` and return
    /// the syscall's result.
    fn reap_zombie(&mut self, tid: pid_t, vw: &VWait, i: usize) -> Result<i64> {
        let z = self.vzombies[i].clone();
        let pid = self.tracee_pid(z.key);
        let t = self.task_mut(tid)?;
        if vw.nr == WAIT4 {
            if vw.out != 0 {
                t.mem.write(vw.out, &z.status.to_ne_bytes())?;
            }
        } else if vw.out != 0 {
            let mut info = [0u8; 128];
            let (code, status) = if z.status & 0x7f == 0 {
                (CLD_EXITED, (z.status >> 8) & 0xff)
            } else if z.status & 0x80 != 0 {
                (CLD_DUMPED, z.status & 0x7f)
            } else {
                (CLD_KILLED, z.status & 0x7f)
            };
            info[..4].copy_from_slice(&libc::SIGCHLD.to_ne_bytes());
            info[8..12].copy_from_slice(&code.to_ne_bytes());
            info[16..20].copy_from_slice(&pid.to_ne_bytes());
            info[20..24].copy_from_slice(&unsafe { libc::getuid() }.to_ne_bytes());
            info[24..28].copy_from_slice(&status.to_ne_bytes());
            t.mem.write(vw.out, &info)?;
        }
        if vw.rusage != 0 {
            t.mem.write(vw.rusage, &[0u8; 144])?;
        }
        if vw.options & WNOWAIT == 0 {
            self.vzombies.remove(i);
        }
        Ok(if vw.nr == WAIT4 { pid as i64 } else { 0 })
    }

    /// Write the result of a wait that reports a stopped or continued
    /// child and return the syscall's result.
    fn report_jobstop(&mut self, tid: pid_t, vw: &VWait, i: usize, continued: bool) -> Result<i64> {
        let j = if continued { self.jobconts[i].clone() } else { self.jobstops[i].clone() };
        let pid = self.tracee_pid(j.key);
        let t = self.task_mut(tid)?;
        if vw.nr == WAIT4 {
            let status: i32 = if continued { 0xffff } else { 0x7f | (j.sig << 8) };
            if vw.out != 0 {
                t.mem.write(vw.out, &status.to_ne_bytes())?;
            }
        } else if vw.out != 0 {
            let mut info = [0u8; 128];
            let code = if continued { CLD_CONTINUED } else { CLD_STOPPED };
            info[..4].copy_from_slice(&libc::SIGCHLD.to_ne_bytes());
            info[8..12].copy_from_slice(&code.to_ne_bytes());
            info[16..20].copy_from_slice(&pid.to_ne_bytes());
            info[20..24].copy_from_slice(&unsafe { libc::getuid() }.to_ne_bytes());
            info[24..28].copy_from_slice(&j.sig.to_ne_bytes());
            t.mem.write(vw.out, &info)?;
        }
        if vw.rusage != 0 {
            t.mem.write(vw.rusage, &[0u8; 144])?;
        }
        if vw.options & WNOWAIT == 0 {
            if continued {
                self.jobconts.remove(i);
            } else {
                self.jobstops.remove(i);
            }
        }
        Ok(if vw.nr == WAIT4 { pid as i64 } else { 0 })
    }

    /// Send the parent of `key` a `SIGCHLD` with the given code/status,
    /// unless it ignores the signal.
    fn notify_parent(&mut self, key: Ltid, parent: Ltid, code: i32, status: i32) -> Result<()> {
        let Some(target) = self.signal_target(parent, None, libc::SIGCHLD) else { return Ok(()) };
        let ignored = ptrace::ignored_signals(target) & (1u64 << (libc::SIGCHLD - 1)) != 0;
        if ignored || ptrace::pending_signals(target) & (1u64 << (libc::SIGCHLD - 1)) != 0 {
            return Ok(());
        }
        let mut info = [0u8; 128];
        info[..4].copy_from_slice(&libc::SIGCHLD.to_ne_bytes());
        info[8..12].copy_from_slice(&code.to_ne_bytes());
        info[16..20].copy_from_slice(&self.tracee_pid(key).to_ne_bytes());
        info[20..24].copy_from_slice(&unsafe { libc::getuid() }.to_ne_bytes());
        info[24..28].copy_from_slice(&status.to_ne_bytes());
        let tgid = self.task(target)?.tgid;
        ptrace::tgkill(tgid, target, libc::SIGCHLD)?;
        let ltid = self.task(target)?.ltid;
        self.siginfo_queue.push((ltid, libc::SIGCHLD, info));
        Ok(())
    }

    /// Process `key` received a stop signal with the default action: it is
    /// stopped until `SIGCONT`.
    pub fn on_process_stop(&mut self, key: Ltid, sig: i32) -> Result<()> {
        if !self.stopped_procs.insert(key) {
            return Ok(());
        }
        let Some(&parent) = self.parent_of.get(&key) else { return Ok(()) };
        if !self.proc_alive(parent) {
            return Ok(());
        }
        let leader = self.ltids[&key];
        let pgid = host_pgid(leader).unwrap_or(leader);
        self.jobconts.retain(|j| j.key != key);
        self.jobstops.push(VJobStop { key, parent, sig, pgid });
        self.notify_parent(key, parent, CLD_STOPPED, sig)
    }

    /// Process `key` is continued (`SIGCONT` pending, or killed).
    pub fn on_process_cont(&mut self, key: Ltid, report: bool) -> Result<()> {
        if !self.stopped_procs.remove(&key) {
            return Ok(());
        }
        self.jobstops.retain(|j| j.key != key);
        if !report {
            return Ok(());
        }
        let Some(&parent) = self.parent_of.get(&key) else { return Ok(()) };
        if !self.proc_alive(parent) {
            return Ok(());
        }
        let leader = self.ltids[&key];
        let pgid = host_pgid(leader).unwrap_or(leader);
        self.jobconts.push(VJobStop { key, parent, sig: libc::SIGCONT, pgid });
        self.notify_parent(key, parent, CLD_CONTINUED, libc::SIGCONT)
    }

    /// A stopped process with `SIGCONT` or `SIGKILL` pending becomes
    /// runnable again (the signal is then delivered normally).
    pub fn proc_check_stopped(&mut self) -> Result<()> {
        let keys: Vec<Ltid> = self.stopped_procs.iter().copied().collect();
        for key in keys {
            let Some(&leader) = self.ltids.get(&key) else { continue };
            let tids: Vec<pid_t> = self.tasks.values().filter(|t| t.tgid == leader && t.is_alive()).map(|t| t.tid).collect();
            let cont = 1u64 << (libc::SIGCONT - 1);
            let kill = 1u64 << (libc::SIGKILL - 1);
            let pending = tids.iter().map(|&t| ptrace::pending_signals(t)).fold(0, |a, b| a | b);
            if pending & kill != 0 || tids.is_empty() {
                self.on_process_cont(key, false)?;
            } else if pending & cont != 0 {
                self.on_process_cont(key, true)?;
            }
        }
        Ok(())
    }

    /// Re-evaluate the emulated wait of a virtually blocked thread.
    pub fn proc_poll_wait(&mut self, tid: pid_t) -> Result<()> {
        let Some(vw) = self.task(tid)?.vwait else { return Ok(()) };
        let key = self.proc_key(tid)?;
        match self.eval_wait(key, &vw) {
            WaitOutcome::Reap(i) => {
                let ret = self.reap_zombie(tid, &vw, i)?;
                self.task_mut(tid)?.vwait = None;
                return self.vt_wake(tid, ret);
            }
            WaitOutcome::Stopped(i) => {
                let ret = self.report_jobstop(tid, &vw, i, false)?;
                self.task_mut(tid)?.vwait = None;
                return self.vt_wake(tid, ret);
            }
            WaitOutcome::Continued(i) => {
                let ret = self.report_jobstop(tid, &vw, i, true)?;
                self.task_mut(tid)?.vwait = None;
                return self.vt_wake(tid, ret);
            }
            WaitOutcome::NoChildren => {
                self.task_mut(tid)?.vwait = None;
                return self.vt_wake(tid, -(libc::ECHILD as i64));
            }
            WaitOutcome::Nothing => {}
        }
        // A pending signal interrupts the wait: EINTR for a handler without
        // SA_RESTART, otherwise the syscall is restarted after the signal.
        let pending = ptrace::interrupting_signals(tid);
        if pending == 0 {
            return Ok(());
        }
        if pending & (1u64 << (libc::SIGKILL - 1)) != 0 {
            // Nothing to ask the tracee: it dies as soon as it runs.
            self.task_mut(tid)?.vwait = None;
            return self.vt_wake(tid, -(libc::EINTR as i64));
        }
        let sig = pending.trailing_zeros() as i32 + 1;
        let queried: Result<(u64, u64)> = (|| {
            let t = self.task_mut(tid)?;
            let mut r = Remote::new(t)?;
            let old = r.alloc_scratch(32)?;
            let _ = r.syscall(RT_SIGACTION, &[sig as u64, 0, old, 8])?;
            let handler = r.task.mem.read_u64(old)?;
            let flags = r.task.mem.read_u64(old + 8)?;
            r.restore()?;
            Ok((handler, flags))
        })();
        let (handler, flags) = match queried {
            Ok(v) => v,
            Err(e) if e.is::<remote::TaskDied>() => {
                let t = self.task_mut(tid)?;
                t.vwait = None;
                t.vblock = None;
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let has_handler = handler > 1;
        if has_handler && flags & SA_RESTART == 0 {
            self.task_mut(tid)?.vwait = None;
            self.vt_wake(tid, -(libc::EINTR as i64))
        } else {
            self.vt_restart(tid)
        }
    }

    /// Called when process `key` has fully exited and been reaped.
    pub fn on_process_exit(&mut self, key: Ltid) -> Result<()> {
        self.exited_procs.insert(key);
        // Children that asked for a parent-death signal get it now (the
        // kernel would send it with SI_USER from the dying parent).
        let orphans: Vec<(Ltid, i32)> = self
            .parent_of
            .iter()
            .filter(|(_, p)| **p == key)
            .filter_map(|(c, _)| self.pdeathsig.get(c).map(|s| (*c, *s)))
            .collect();
        for (child, sig) in orphans {
            if !self.proc_alive(child) {
                continue;
            }
            if let Some(target) = self.signal_target(child, None, sig) {
                let mut info = [0u8; 128];
                info[..4].copy_from_slice(&sig.to_ne_bytes());
                info[16..20].copy_from_slice(&self.tracee_pid(key).to_ne_bytes());
                info[20..24].copy_from_slice(&unsafe { libc::getuid() }.to_ne_bytes());
                let tgid = self.task(target)?.tgid;
                ptrace::tgkill(tgid, target, sig)?;
                let ltid = self.task(target)?.ltid;
                self.siginfo_queue.push((ltid, sig, info));
            }
        }
        self.pdeathsig.remove(&key);

        self.stopped_procs.remove(&key);
        self.jobstops.retain(|j| j.key != key && j.parent != key);
        self.jobconts.retain(|j| j.key != key && j.parent != key);
        // Its children are reparented to init, which reaps them.
        self.vzombies.retain(|z| z.parent != key);
        self.parent_of.retain(|_, p| *p != key);
        self.vtimers.retain(|tm| tm.proc_key != key);
        let Some(parent) = self.parent_of.remove(&key) else { return Ok(()) };
        if !self.proc_alive(parent) {
            return Ok(());
        }
        let leader = self.ltids[&key];
        let status = self
            .exit_override
            .remove(&leader)
            .or_else(|| self.tasks.get(&leader).and_then(|t| t.exit_status))
            .unwrap_or(0);
        let pgid = host_pgid(leader).unwrap_or(leader);
        let Some(target) = self.signal_target(parent, None, libc::SIGCHLD) else { return Ok(()) };
        let ignored = ptrace::ignored_signals(target) & (1u64 << (libc::SIGCHLD - 1)) != 0;
        if !ignored {
            self.vzombies.push(VZombie { key, parent, status, pgid });
            if ptrace::pending_signals(target) & (1u64 << (libc::SIGCHLD - 1)) == 0 {
                let (code, st) = if status & 0x7f == 0 {
                    (CLD_EXITED, (status >> 8) & 0xff)
                } else if status & 0x80 != 0 {
                    (CLD_DUMPED, status & 0x7f)
                } else {
                    (CLD_KILLED, status & 0x7f)
                };
                let mut info = [0u8; 128];
                info[..4].copy_from_slice(&libc::SIGCHLD.to_ne_bytes());
                info[8..12].copy_from_slice(&code.to_ne_bytes());
                info[16..20].copy_from_slice(&self.tracee_pid(key).to_ne_bytes());
                info[20..24].copy_from_slice(&unsafe { libc::getuid() }.to_ne_bytes());
                info[24..28].copy_from_slice(&st.to_ne_bytes());
                let tgid = self.task(target)?.tgid;
                ptrace::tgkill(tgid, target, libc::SIGCHLD)?;
                let ltid = self.task(target)?.ltid;
                self.siginfo_queue.push((ltid, libc::SIGCHLD, info));
            }
        }
        Ok(())
    }

    /// Notice processes that have exited completely.
    pub fn proc_check_exits(&mut self) -> Result<()> {
        let leaders: Vec<(pid_t, Ltid)> = self
            .tasks
            .values()
            .filter(|t| t.tid == t.tgid && !t.is_alive() && !self.exited_procs.contains(&t.ltid) && !self.zombies.contains(&t.tid))
            .map(|t| (t.tid, t.ltid))
            .collect();
        for (leader, key) in leaders {
            if self.tasks.values().any(|t| t.tgid == leader && t.is_alive()) {
                continue;
            }
            self.on_process_exit(key)?;
        }
        Ok(())
    }
}

#[allow(dead_code)]
pub const SYS_CLONE_NR: u64 = remote::SYS_CLONE;
