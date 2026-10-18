//! Tracee memory access through `/proc/<pid>/mem`, with a PEEK/POKE fallback.
//!
//! Writes through /proc/pid/mem use `FOLL_FORCE`, so they can modify read-only
//! private mappings (code pages, the vDSO) with copy-on-write semantics, which
//! is exactly what we need for patching instructions.

use anyhow::{anyhow, Context, Result};
use libc::pid_t;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;

use crate::ptrace;

pub struct Mem {
    pid: pid_t,
    file: Option<File>,
}

impl Mem {
    pub fn open(pid: pid_t) -> Mem {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("/proc/{pid}/mem"))
            .ok();
        Mem { pid, file }
    }

    pub fn pid(&self) -> pid_t {
        self.pid
    }

    /// Read as much of `buf` as is mapped from `addr` on (a page boundary
    /// may end the mapping); returns the number of bytes read.
    pub fn read_partial(&self, addr: u64, buf: &mut [u8]) -> usize {
        if self.read(addr, buf).is_ok() {
            return buf.len();
        }
        let page_end = ((addr | 0xfff) + 1 - addr) as usize;
        let n = page_end.min(buf.len());
        if n < buf.len() && self.read(addr, &mut buf[..n]).is_ok() {
            return n;
        }
        0
    }

    pub fn read(&self, addr: u64, buf: &mut [u8]) -> Result<()> {
        if let Some(f) = &self.file {
            if f.read_exact_at(buf, addr).is_ok() {
                return Ok(());
            }
        }
        // Fallback: word-wise peek.
        let mut off = 0usize;
        while off < buf.len() {
            let a = (addr as usize + off) & !7;
            let w = ptrace::peek(self.pid, a)
                .with_context(|| format!("read tracee memory at {:#x}", addr as usize + off))?;
            let bytes = w.to_ne_bytes();
            let start = (addr as usize + off) - a;
            let n = (8 - start).min(buf.len() - off);
            buf[off..off + n].copy_from_slice(&bytes[start..start + n]);
            off += n;
        }
        Ok(())
    }

    pub fn write(&self, addr: u64, data: &[u8]) -> Result<()> {
        if let Some(f) = &self.file {
            if f.write_all_at(data, addr).is_ok() {
                return Ok(());
            }
        }
        let mut off = 0usize;
        while off < data.len() {
            let a = (addr as usize + off) & !7;
            let old = ptrace::peek(self.pid, a)?;
            let mut bytes = old.to_ne_bytes();
            let start = (addr as usize + off) - a;
            let n = (8 - start).min(data.len() - off);
            bytes[start..start + n].copy_from_slice(&data[off..off + n]);
            ptrace::poke(self.pid, a, u64::from_ne_bytes(bytes))
                .with_context(|| format!("write tracee memory at {:#x}", addr as usize + off))?;
            off += n;
        }
        Ok(())
    }

    pub fn read_u64(&self, addr: u64) -> Result<u64> {
        let mut b = [0u8; 8];
        self.read(addr, &mut b)?;
        Ok(u64::from_ne_bytes(b))
    }

    pub fn write_u64(&self, addr: u64, v: u64) -> Result<()> {
        self.write(addr, &v.to_ne_bytes())
    }

    /// Read a NUL-terminated string (bounded).
    pub fn read_cstr(&self, addr: u64, max: usize) -> Result<String> {
        if addr == 0 {
            return Ok(String::new());
        }
        let mut out = Vec::new();
        let mut chunk = [0u8; 64];
        let mut a = addr;
        while out.len() < max {
            // Don't cross into an unmapped page: read in page-bounded chunks.
            let page_left = 4096 - (a as usize & 4095);
            let n = chunk.len().min(page_left);
            if self.read(a, &mut chunk[..n]).is_err() {
                if out.is_empty() {
                    return Err(anyhow!("unreadable string at {addr:#x}"));
                }
                break;
            }
            if let Some(p) = chunk[..n].iter().position(|&c| c == 0) {
                out.extend_from_slice(&chunk[..p]);
                break;
            }
            out.extend_from_slice(&chunk[..n]);
            a += n as u64;
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

/// A mapping from /proc/<pid>/maps.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Mapping {
    pub start: u64,
    pub end: u64,
    pub perms: String,
    pub offset: u64,
    pub dev: String,
    pub inode: u64,
    pub path: String,
}

impl Mapping {
    pub fn is_shared(&self) -> bool {
        self.perms.as_bytes().get(3) == Some(&b's')
    }
    pub fn is_writable(&self) -> bool {
        self.perms.as_bytes().get(1) == Some(&b'w')
    }
    pub fn prot(&self) -> i32 {
        let b = self.perms.as_bytes();
        let mut p = 0;
        if b.first() == Some(&b'r') {
            p |= libc::PROT_READ;
        }
        if b.get(1) == Some(&b'w') {
            p |= libc::PROT_WRITE;
        }
        if b.get(2) == Some(&b'x') {
            p |= libc::PROT_EXEC;
        }
        p
    }
}

pub fn read_maps(pid: pid_t) -> Result<Vec<Mapping>> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/maps"))
        .with_context(|| format!("read /proc/{pid}/maps"))?;
    let mut out = Vec::new();
    for line in s.lines() {
        let mut it = line.split_whitespace();
        let range = it.next().unwrap_or("");
        let perms = it.next().unwrap_or("").to_string();
        let offset = u64::from_str_radix(it.next().unwrap_or("0"), 16).unwrap_or(0);
        let dev = it.next().unwrap_or("").to_string();
        let inode = it.next().unwrap_or("0").parse().unwrap_or(0);
        let path = it.collect::<Vec<_>>().join(" ");
        let (s, e) = range.split_once('-').unwrap_or(("0", "0"));
        out.push(Mapping {
            start: u64::from_str_radix(s, 16).unwrap_or(0),
            end: u64::from_str_radix(e, 16).unwrap_or(0),
            perms,
            offset,
            dev,
            inode,
            path,
        });
    }
    Ok(out)
}
