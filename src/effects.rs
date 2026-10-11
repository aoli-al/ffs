//! Syscall output recording for the native x86-64 ABI.
//!
//! Replay executes live syscalls and compares their observed outputs after
//! the same time/randomness/identity virtualization used during recording.
//! Recorded buffers are evidence to check, never replacement input.
use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};

use crate::ptrace::Regs;
use crate::session::{Event, Session};
use crate::task::{Ltid, Task};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Output {
    pub addr: u64,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Effects {
    pub out: Vec<Output>,
}

pub fn refused_syscall(nr: u64) -> bool {
    // aio setup/submit, perf events, userfaultfd,
    // io_uring setup/enter/register. Also reject inherited ring use.
    matches!(nr, 206 | 209 | 298 | 323 | 425 | 426 | 427)
}

fn metadata_query(nr: u64) -> bool {
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
        out.push(Output { addr, bytes });
    }
    Ok(Some(Effects { out }))
}

/// Disk latency in a synchronous syscall must not create a scheduling point.
/// Unknown operations retain the existing blocking detection; this deliberately
/// does not claim to emulate futex or external-I/O readiness.
pub fn may_block(t: &mut Task, nr: u64) -> Result<bool> {
    if metadata_query(nr)
        || matches!(
            nr,
            3 | 8
                | 9
                | 10
                | 11
                | 12
                | 24
                | 32
                | 33
                | 39
                | 102
                | 104
                | 107
                | 108
                | 186
                | 218
                | 228
                | 229
                | 273
                | 292
                | 318
                | 334
        )
    {
        return Ok(false);
    }
    if matches!(
        nr,
        0 | 1 | 17 | 18 | 19 | 20 | 78 | 217 | 295 | 296 | 327 | 328
    ) {
        let fd = t.regs()?.rdi;
        return Ok(!std::fs::metadata(format!("/proc/{}/fd/{fd}", t.tid))
            .map(|m| m.is_file())
            .unwrap_or(false));
    }
    Ok(true)
}
