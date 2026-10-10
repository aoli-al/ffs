//! Private PID namespace for the tracees, so that thread/process ids can be
//! chosen by the tracer (CRIU's `ns_last_pid` trick).
//!
//! `ffs` creates a user namespace (identity uid/gid mapping, full capabilities
//! inside) and a PID namespace for its children.  A tiny init process is
//! forked into the namespace; it keeps the namespace alive, reaps orphans,
//! and on request writes `/proc/sys/kernel/ns_last_pid`, which decides the
//! pid the kernel hands out next.  Because only one tracee thread runs at a
//! time (and it is stopped while we steer), the next pid allocated inside
//! the namespace is exactly the one we asked for.
//!
//! Live sessions use canonical ids `CANONICAL_BASE + ltid`; frozen
//! checkpoints are kept out of the way in a high range.  Thus a thread has
//! the same tid in every branch and in a cold replay.

use anyhow::{anyhow, bail, Context, Result};
use libc::pid_t;
use std::cell::Cell;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};

use crate::task::Ltid;

pub const CANONICAL_BASE: pid_t = 10_000;
const FROZEN_BASE: pid_t = 1_000_000;
const FROZEN_STRIDE: pid_t = 4096;

pub struct PidNs {
    cmd: std::fs::File,
    ack: std::fs::File,
    pub init_pid: pid_t,
    frozen_cursor: Cell<pid_t>,
}

pub fn canonical_tid(ltid: Ltid) -> pid_t {
    CANONICAL_BASE + ltid as pid_t
}

fn write_file(path: &str, data: &str) -> Result<()> {
    std::fs::write(path, data).with_context(|| format!("write {path}"))
}

impl PidNs {
    /// Create the namespaces and the init helper.  Must be called while the
    /// process is still single-threaded and before any tracee is spawned.
    pub fn setup() -> Result<PidNs> {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        if unsafe { libc::unshare(libc::CLONE_NEWUSER) } < 0 {
            bail!("unshare(CLONE_NEWUSER): {}", std::io::Error::last_os_error());
        }
        write_file("/proc/self/setgroups", "deny")?;
        write_file("/proc/self/gid_map", &format!("{gid} {gid} 1\n"))?;
        write_file("/proc/self/uid_map", &format!("{uid} {uid} 1\n"))?;
        // Entering the user namespace clears the dumpable flag; tracees open
        // our file descriptors through /proc/<pid>/fd, which needs it.
        unsafe {
            libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0);
        }
        if unsafe { libc::unshare(libc::CLONE_NEWPID) } < 0 {
            bail!("unshare(CLONE_NEWPID): {}", std::io::Error::last_os_error());
        }
        let mut cmd_fds = [0i32; 2];
        let mut ack_fds = [0i32; 2];
        unsafe {
            if libc::pipe2(cmd_fds.as_mut_ptr(), libc::O_CLOEXEC) < 0 || libc::pipe2(ack_fds.as_mut_ptr(), libc::O_CLOEXEC) < 0 {
                bail!("pipe2: {}", std::io::Error::last_os_error());
            }
        }
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            bail!("fork init: {}", std::io::Error::last_os_error());
        }
        if pid == 0 {
            // The namespace's init.  Only async-signal-safe calls.
            unsafe {
                libc::close(cmd_fds[1]);
                libc::close(ack_fds[0]);
                libc::signal(libc::SIGCHLD, libc::SIG_IGN); // auto-reap orphans
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                libc::prctl(libc::PR_SET_NAME, c"ffs-ns-init".as_ptr());
                let path = b"/proc/sys/kernel/ns_last_pid\0";
                loop {
                    let mut buf = [0u8; 8];
                    let n = libc::read(cmd_fds[0], buf.as_mut_ptr() as *mut libc::c_void, 8);
                    if n <= 0 {
                        libc::_exit(0);
                    }
                    let v = i64::from_ne_bytes(buf);
                    let mut status = 0u8;
                    let fd = libc::open(path.as_ptr() as *const libc::c_char, libc::O_WRONLY);
                    if fd < 0 {
                        status = 1;
                    } else {
                        // decimal formatting without allocation
                        let mut digits = [0u8; 24];
                        let mut i = digits.len();
                        let mut x = if v < 0 { 0 } else { v as u64 };
                        if x == 0 {
                            i -= 1;
                            digits[i] = b'0';
                        }
                        while x > 0 {
                            i -= 1;
                            digits[i] = b'0' + (x % 10) as u8;
                            x /= 10;
                        }
                        let w = libc::write(fd, digits[i..].as_ptr() as *const libc::c_void, digits.len() - i);
                        if w < 0 {
                            status = 2;
                        }
                        libc::close(fd);
                    }
                    libc::write(ack_fds[1], &status as *const u8 as *const libc::c_void, 1);
                }
            }
        }
        unsafe {
            libc::close(cmd_fds[0]);
            libc::close(ack_fds[1]);
        }
        let cmd = unsafe { std::fs::File::from(OwnedFd::from_raw_fd(cmd_fds[1])) };
        let ack = unsafe { std::fs::File::from(OwnedFd::from_raw_fd(ack_fds[0])) };
        // ffs keeps the capabilities of the user namespace: the tracee is
        // spawned with them so that it can mount its own /proc, and gets
        // descriptors of ours over a Unix socket (`fdpass.rs`) rather than
        // through /proc/<ffs>/fd, which its /proc does not show anyway.
        let ns = PidNs {
            cmd,
            ack,
            init_pid: pid,
            frozen_cursor: Cell::new(FROZEN_BASE),
        };
        // Probe: make sure steering works.
        ns.set_next_pid(CANONICAL_BASE).context("pid steering probe")?;
        Ok(ns)
    }

    /// Make `pid` the next pid allocated in the namespace.
    pub fn set_next_pid(&self, pid: pid_t) -> Result<()> {
        let v = (pid as i64 - 1).to_ne_bytes();
        (&self.cmd).write_all(&v).context("send to ns init")?;
        let mut st = [0u8; 1];
        (&self.ack).read_exact(&mut st).context("ack from ns init")?;
        if st[0] != 0 {
            bail!("ns init could not write ns_last_pid (status {})", st[0]);
        }
        Ok(())
    }

    /// Steer the next allocation to the canonical id of `ltid`.
    pub fn steer_canonical(&self, ltid: Ltid) -> Result<()> {
        self.set_next_pid(canonical_tid(ltid))
    }

    /// Steer allocations into the frozen range (a fresh block each time).
    pub fn steer_frozen(&self) -> Result<()> {
        let v = self.frozen_cursor.get();
        self.frozen_cursor.set(v + FROZEN_STRIDE);
        self.set_next_pid(v)
    }

}

impl Drop for PidNs {
    fn drop(&mut self) {
        // Killing init kills the namespace; the kernel then waits for every
        // process in it to be reaped, which for our tracees is our job.
        unsafe {
            libc::kill(self.init_pid, libc::SIGKILL);
            loop {
                let mut status = 0;
                let r = libc::waitpid(-1, &mut status, libc::__WALL);
                if r < 0 || r == self.init_pid {
                    break;
                }
                if libc::WIFSTOPPED(status) {
                    libc::ptrace(libc::PTRACE_DETACH, r, 0, 0);
                }
            }
        }
    }
}

/// The pid of `tid` as seen inside the innermost pid namespace.
pub fn ns_pid(tid: pid_t) -> Option<pid_t> {
    let s = std::fs::read_to_string(format!("/proc/{tid}/status")).ok()?;
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("NSpid:") {
            return v.split_whitespace().last()?.parse().ok();
        }
    }
    None
}

#[allow(dead_code)]
pub fn no_ns() -> Result<PidNs> {
    Err(anyhow!("pid namespace disabled"))
}
