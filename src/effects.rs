//! Syscall output recording for the native x86-64 ABI.
//!
//! Replay executes live syscalls and compares their observed outputs after
//! the same time/randomness/identity virtualization used during recording.
//! Recorded buffers are evidence to check, never replacement input.
use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};

use crate::ptrace::Regs;
use crate::session::{Event, Session};
use crate::task::{Ltid, Task, BlockClass};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Output {
    pub addr: u64,
    /// The bytes themselves, up to `INLINE_BYTES` (enough to see what was read).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bytes: Vec<u8>,
    /// For larger buffers, (length, FNV-1a 64 hash) instead: a test that
    /// moves blobs around would otherwise record gigabytes of JSON, and a
    /// replay only needs to know whether the bytes were the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<(usize, u64)>,
}

/// Largest output stored byte for byte.
pub const INLINE_BYTES: usize = 512;

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

impl Output {
    pub fn new(addr: u64, bytes: Vec<u8>) -> Output {
        if bytes.len() > INLINE_BYTES {
            Output { addr, digest: Some((bytes.len(), fnv1a64(&bytes))), bytes: Vec::new() }
        } else {
            Output { addr, bytes, digest: None }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Effects {
    pub out: Vec<Output>,
}

pub fn refused_syscall(nr: u64) -> bool {
    // aio setup/submit, perf events, userfaultfd: these complete work
    // outside traced syscall boundaries.  io_uring is run synchronously
    // instead (see `Session::uring_pre_syscall`).
    matches!(nr, 206 | 209 | 298 | 323)
}

pub fn metadata_query(nr: u64) -> bool {
    // stat/fstat/lstat, uname, readlink, sysinfo, statfs/fstatfs,
    // newfstatat, readlinkat, statx. These do not advance fd state.
    matches!(nr, 4 | 5 | 6 | 63 | 89 | 99 | 137 | 138 | 262 | 267 | 332)
}

fn supported(nr: u64) -> bool {
    metadata_query(nr)
        || matches!(
            nr,
            0 | 17 | 19 | 78 | 96 | 201 | 217 | 228 | 229 | 295 | 310 | 318 | 327
        )
}

// Bound allocations explicitly; never silently truncate a recorded effect.
const MAX_OUTPUT: usize = 64 * 1024 * 1024;

fn ranges(t: &Task, r: &Regs, nr: u64, ret: i64) -> Result<Vec<(u64, usize)>> {
    if ret < 0 {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut add = |addr: u64, len: usize| -> Result<()> {
        ensure!(
            len <= MAX_OUTPUT,
            "syscall {nr} output exceeds {MAX_OUTPUT} bytes"
        );
        if len > 0 {
            ensure!(
                addr != 0 && addr.checked_add(len as u64).is_some(),
                "invalid syscall output range"
            );
            out.push((addr, len));
        }
        Ok(())
    };
    match nr {
        0 | 17 | 78 | 217 => {
            ensure!(
                ret as u64 <= r.rdx,
                "syscall {nr} returned more than its buffer size"
            );
            add(r.rsi, ret as usize)?;
        }
        318 => {
            ensure!(
                ret as u64 <= r.rsi,
                "getrandom returned more than its buffer size"
            );
            add(r.rdi, ret as usize)?;
        }
        19 | 295 | 310 | 327 => {
            ensure!(
                r.rdx <= 1024 && ret as usize <= MAX_OUTPUT,
                "invalid vectored read size"
            );
            let mut left = ret as usize;
            for i in 0..r.rdx {
                if left == 0 {
                    break;
                }
                let at = r
                    .rsi
                    .checked_add(i * 16)
                    .ok_or_else(|| anyhow::anyhow!("iovec address overflow"))?;
                let addr = t.mem.read_u64(at)?;
                let len = t.mem.read_u64(at + 8)?.min(left as u64) as usize;
                add(addr, len)?;
                left -= len;
            }
            ensure!(left == 0, "vectored read returned more than its iovecs");
        }
        4..=6 => add(r.rsi, 144)?, // struct stat (native x86-64)
        262 => add(r.rdx, 144)?,
        332 => add(r.r8, 256)?,        // struct statx
        63 => add(r.rdi, 390)?,        // struct utsname, six 65-byte fields
        99 => add(r.rdi, 112)?,        // struct sysinfo
        137 | 138 => add(r.rsi, 120)?, // struct statfs
        89 => {
            ensure!(
                ret as u64 <= r.rdx,
                "readlink returned more than its buffer size"
            );
            add(r.rsi, ret as usize)?;
        }
        267 => {
            ensure!(
                ret as u64 <= r.r10,
                "readlinkat returned more than its buffer size"
            );
            add(r.rdx, ret as usize)?;
        }
        96 => {
            if r.rdi != 0 {
                add(r.rdi, 16)?;
            }
            if r.rsi != 0 {
                add(r.rsi, 8)?;
            }
        }
        201 => {
            if r.rdi != 0 {
                add(r.rdi, 8)?;
            }
        }
        228 | 229 => {
            if r.rsi != 0 {
                add(r.rsi, 16)?;
            }
        }
        _ => bail!("no output description for syscall {nr}"),
    }
    Ok(out)
}

pub fn capture(session: &mut Session, ltid: Ltid, event: &Event) -> Result<Option<Effects>> {
    let Event::SyscallExit { nr, ret, .. } = event else {
        return Ok(None);
    };
    if !supported(*nr) {
        return Ok(None);
    }
    let t = session.by_ltid(ltid)?;
    let r = t.regs()?;
    let mut out = Vec::new();
    for (addr, len) in ranges(t, &r, *nr, *ret)? {
        let mut bytes = vec![0; len];
        t.mem.read(addr, &mut bytes)?;
        out.push(Output::new(addr, bytes));
    }
    Ok(Some(Effects { out }))
}

/// Classify what syscall `nr` can block on (see `BlockClass`).  `peers`
/// are the socket inodes of tracee-created socket pairs.
pub fn block_class(t: &mut Task, nr: u64, peers: &std::collections::BTreeMap<u64, u64>) -> Result<BlockClass> {
    const READ_LIKE: [u64; 8] = [0, 17, 19, 45, 47, 295, 299, 327];
    const WRITE_LIKE: [u64; 8] = [1, 18, 20, 44, 46, 296, 307, 328];
    if metadata_query(nr)
        || matches!(
            nr,
            3 | 8 | 9 | 10 | 11 | 12 | 24 | 32 | 33 | 39 | 102 | 104 | 107 | 108 | 186 | 218 | 228 | 229 | 273 | 292 | 318 | 334
        )
    {
        return Ok(BlockClass::Never);
    }
    // Waits that only another tracee or a (virtual) timer can end: futex,
    // futex_waitv, pause, rt_sigsuspend, rt_sigtimedwait, wait4, waitid,
    // semop, semtimedop, msgrcv, flock, mq_timedsend/receive.
    if matches!(nr, 34 | 61 | 65 | 70 | 73 | 128 | 130 | 202 | 220 | 242 | 243 | 247 | 449) {
        return Ok(BlockClass::Tracee);
    }
    // Descriptor waits: poll, select, accept, pselect6, ppoll, epoll_wait,
    // epoll_pwait, accept4, epoll_pwait2.
    if matches!(nr, 7 | 23 | 43 | 270 | 271 | 232 | 281 | 288 | 441) {
        return Ok(BlockClass::Fd);
    }
    if READ_LIKE.contains(&nr) || WRITE_LIKE.contains(&nr) {
        let fd = t.regs()?.rdi;
        let link = std::fs::read_link(format!("/proc/{}/fd/{fd}", t.tid)).ok();
        let Some(link) = link else { return Ok(BlockClass::External) };
        let l = link.to_string_lossy();
        if l.starts_with("pipe:") || l.starts_with("anon_inode:[eventfd]") || l.starts_with("anon_inode:[timerfd]")
            || l.starts_with("anon_inode:[signalfd]")
        {
            return Ok(BlockClass::Fd);
        }
        if let Some(ino) = l.strip_prefix("socket:[").and_then(|r| r.strip_suffix(']')).and_then(|n| n.parse::<u64>().ok()) {
            return Ok(if peers.contains_key(&ino) { BlockClass::Fd } else { BlockClass::External });
        }
        if std::fs::metadata(format!("/proc/{}/fd/{fd}", t.tid)).map(|m| m.is_file()).unwrap_or(false) {
            return Ok(BlockClass::Never);
        }
        return Ok(BlockClass::External);
    }
    Ok(BlockClass::External)
}
