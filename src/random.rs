//! Virtualized randomness.  rr records the bytes the kernel hands out and
//! replays them; ffs re-executes, so instead every source of kernel
//! randomness is replaced by a deterministic stream seeded from the trace
//! seed: `getrandom`, reads from `/dev/urandom` and `/dev/random`, and the
//! `AT_RANDOM` bytes the kernel puts on the stack at `execve` (glibc derives
//! its stack-protector canary and pointer guard from them).
//!
//! The generator state is part of the session, so a checkpoint copies it
//! and every branch sees the same values at the same program points.

use anyhow::Result;
use libc::pid_t;
use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;
use std::collections::BTreeSet;

use crate::session::Session;
use crate::task::Ltid;

/// Keeps the random stream distinct from the scheduler's for the same seed.
const SEED_DOMAIN: u64 = 0x5eed_0fff5;

#[derive(Clone)]
pub struct VirtualRandom {
    rng: ChaCha8Rng,
    /// (process, fd) pairs open on a random device.  The process is
    /// identified by the logical id of its initial thread, which is stable
    /// across checkpoints.
    fds: BTreeSet<(Ltid, i32)>,
}

impl VirtualRandom {
    pub fn new(seed: u64) -> VirtualRandom {
        VirtualRandom {
            rng: ChaCha8Rng::seed_from_u64(seed ^ SEED_DOMAIN),
            fds: BTreeSet::new(),
        }
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n];
        self.rng.fill_bytes(&mut v);
        v
    }
}

const SYS_READ: u64 = 0;
const SYS_OPEN: u64 = 2;
const SYS_CLOSE: u64 = 3;
const SYS_PREAD64: u64 = 17;
const SYS_DUP: u64 = 32;
const SYS_DUP2: u64 = 33;
const SYS_FCNTL: u64 = 72;
const SYS_OPENAT: u64 = 257;
const SYS_DUP3: u64 = 292;
const SYS_GETRANDOM: u64 = 318;
const F_DUPFD: u64 = 0;
const F_DUPFD_CLOEXEC: u64 = 1030;
const AT_RANDOM: u64 = 25;

fn is_random_device(tid: pid_t, fd: i64) -> bool {
    match std::fs::read_link(format!("/proc/{tid}/fd/{fd}")) {
        Ok(p) => {
            let p = p.to_string_lossy();
            p == "/dev/urandom" || p == "/dev/random"
        }
        Err(_) => false,
    }
}

impl Session {
    /// Logical id of the initial thread of `tid`'s process.
    pub fn proc_key(&self, tid: pid_t) -> Result<Ltid> {
        let t = self.task(tid)?;
        Ok(self.tasks.get(&t.tgid).map(|l| l.ltid).unwrap_or(t.ltid))
    }

    /// Called at every syscall-exit stop when randomness is virtualized.
    pub fn virtualize_random_result(&mut self, tid: pid_t, nr: u64) -> Result<()> {
        if !matches!(
            nr,
            SYS_READ | SYS_OPEN | SYS_CLOSE | SYS_PREAD64 | SYS_DUP | SYS_DUP2 | SYS_FCNTL | SYS_OPENAT | SYS_DUP3 | SYS_GETRANDOM
        ) {
            return Ok(());
        }
        let key = self.proc_key(tid)?;
        let r = self.task_mut(tid)?.regs()?;
        let ret = r.rax as i64;
        if ret < 0 {
            return Ok(());
        }
        let fd = ret as i32;
        match nr {
            SYS_GETRANDOM if ret > 0 => {
                let bytes = self.vrandom.bytes(ret as usize);
                self.task_mut(tid)?.mem.write(r.rdi, &bytes)?;
            }
            SYS_OPEN | SYS_OPENAT => {
                if is_random_device(tid, ret) {
                    self.vrandom.fds.insert((key, fd));
                } else {
                    self.vrandom.fds.remove(&(key, fd));
                }
            }
            SYS_CLOSE => {
                self.vrandom.fds.remove(&(key, r.rdi as i32));
            }
            SYS_FCNTL if !matches!(r.rsi, F_DUPFD | F_DUPFD_CLOEXEC) => {}
            SYS_DUP | SYS_DUP2 | SYS_DUP3 | SYS_FCNTL => {
                if self.vrandom.fds.contains(&(key, r.rdi as i32)) {
                    self.vrandom.fds.insert((key, fd));
                } else {
                    self.vrandom.fds.remove(&(key, fd));
                }
            }
            SYS_READ | SYS_PREAD64 if ret > 0 && self.vrandom.fds.contains(&(key, r.rdi as i32)) => {
                let bytes = self.vrandom.bytes(ret as usize);
                self.task_mut(tid)?.mem.write(r.rsi, &bytes)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// A new process inherits its parent's random-device descriptors.
    pub fn random_fork(&mut self, parent_key: Ltid, child_key: Ltid) {
        let inherited: Vec<i32> = self
            .vrandom
            .fds
            .iter()
            .filter(|(k, _)| *k == parent_key)
            .map(|(_, fd)| *fd)
            .collect();
        for fd in inherited {
            self.vrandom.fds.insert((child_key, fd));
        }
    }

    /// After `execve`: overwrite the kernel's `AT_RANDOM` bytes before the
    /// new image runs, and forget the process's descriptors (an
    /// approximation: descriptors without `O_CLOEXEC` would survive).
    pub fn random_exec(&mut self, tid: pid_t) -> Result<()> {
        let key = self.proc_key(tid)?;
        self.vrandom.fds.retain(|(k, _)| *k != key);
        let auxv = std::fs::read(format!("/proc/{tid}/auxv"))?;
        for pair in auxv.as_chunks::<16>().0 {
            let kind = u64::from_ne_bytes(pair[..8].try_into().unwrap());
            let val = u64::from_ne_bytes(pair[8..].try_into().unwrap());
            if kind == AT_RANDOM && val != 0 {
                let bytes = self.vrandom.bytes(16);
                self.task_mut(tid)?.mem.write(val, &bytes)?;
                break;
            }
        }
        Ok(())
    }
}
