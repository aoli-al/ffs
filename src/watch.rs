//! Hardware watchpoints as scheduling points.  Tick budgets can only stop a
//! thread at a conditional branch, so a race window between two
//! straight-line stores is nearly unreachable by counting.  x86 debug
//! registers (four of them) trap right after any access to an 8-byte slot,
//! which turns every access to a watched variable into a scheduling point.
//! `ffs explore` samples a few writable globals of the executable per
//! branch (or watches what `--watch` names); the resulting stops are
//! recorded as ordinary preemptions, so traces stay replayable without the
//! watchpoints.

use anyhow::{anyhow, bail, Context, Result};
use libc::pid_t;

use crate::mem::read_maps;
use crate::session::Session;

/// `offsetof(struct user, u_debugreg)` on x86-64.
const DEBUGREG_OFFSET: usize = 848;
pub const TRAP_HWBKPT: i32 = 4;

fn poke_user(tid: pid_t, offset: usize, value: u64) -> Result<()> {
    let r = unsafe { libc::ptrace(libc::PTRACE_POKEUSER, tid, offset as *mut libc::c_void, value as *mut libc::c_void) };
    if r < 0 {
        bail!("PTRACE_POKEUSER({offset}) on {tid}: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// Program the debug registers of a stopped thread: up to four 8-byte
/// slots (aligned), trapping on read or write.
pub fn set_watchpoints(tid: pid_t, addrs: &[u64]) -> Result<()> {
    let mut dr7: u64 = 0;
    // Disable first so that stale addresses never trap.
    poke_user(tid, DEBUGREG_OFFSET + 7 * 8, 0)?;
    for (i, &a) in addrs.iter().take(4).enumerate() {
        if a == 0 {
            continue; // disabled slot
        }
        poke_user(tid, DEBUGREG_OFFSET + i * 8, a & !7)?;
        dr7 |= 1 << (2 * i); // local enable
        dr7 |= 0b11 << (16 + 4 * i); // read or write
        dr7 |= 0b10 << (18 + 4 * i); // 8 bytes
    }
    if dr7 != 0 {
        poke_user(tid, DEBUGREG_OFFSET + 7 * 8, dr7)?;
    }
    Ok(())
}

/// DR7 enabling the given watchpoint slots (same encoding as
/// `set_watchpoints`).
fn dr7_for(addrs: &[u64]) -> u64 {
    let mut dr7 = 0u64;
    for (i, &a) in addrs.iter().take(4).enumerate() {
        if a != 0 {
            dr7 |= 1 << (2 * i);
            dr7 |= 0b11 << (16 + 4 * i);
            dr7 |= 0b10 << (18 + 4 * i);
        }
    }
    dr7
}

/// The debug-register slot a replay uses as an execute breakpoint on the
/// instruction a preemption was recorded at (`Session::run_task_to`).  It
/// is free while fewer than four watchpoints are set.
pub const EXEC_SLOT: usize = 3;

/// Arm `EXEC_SLOT` as an execute breakpoint at `addr` (trapping before the
/// instruction, with the kernel setting RF so it then executes), keeping
/// the watchpoints `watch` enabled.  Unlike an `int3` written into the
/// code it survives the program rewriting that code (a JIT patching its
/// own output) and costs no memory round trips per hit.
pub fn arm_exec_breakpoint(tid: pid_t, addr: u64, watch: &[u64]) -> Result<()> {
    poke_user(tid, DEBUGREG_OFFSET + EXEC_SLOT * 8, addr)?;
    poke_user(tid, DEBUGREG_OFFSET + 7 * 8, dr7_for(watch) | (1 << (2 * EXEC_SLOT)))
}

pub fn disarm_exec_breakpoint(tid: pid_t, watch: &[u64]) -> Result<()> {
    poke_user(tid, DEBUGREG_OFFSET + 7 * 8, dr7_for(watch))
}

/// Which watchpoint slots the last SIGTRAP of `tid` involved (bit mask of
/// DR6's B0..B3, cleared afterwards).  A single-step trap over a watched
/// access reports `TRAP_TRACE` in its siginfo, so the siginfo alone is
/// not enough.
pub fn watchpoint_hits(tid: pid_t) -> u8 {
    let dr6 = unsafe {
        libc::ptrace(
            libc::PTRACE_PEEKUSER,
            tid,
            (DEBUGREG_OFFSET + 6 * 8) as *mut libc::c_void,
            std::ptr::null_mut::<libc::c_void>(),
        )
    };
    let bits = (dr6 & 0xf) as u8;
    if bits != 0 {
        let _ = poke_user(tid, DEBUGREG_OFFSET + 6 * 8, 0);
    }
    bits
}

/// Accesses to one slot after which it stops being a scheduling point for
/// the rest of the execution (a variable polled in a loop would otherwise
/// dominate the schedule and the running time).
pub const MAX_HITS_PER_SLOT: u32 = 500;

/// A writable global of the executable: name, address, size.
#[derive(Clone, Debug)]
pub struct Global {
    pub name: String,
    pub addr: u64,
    pub size: u64,
}

/// Writable object symbols of the main executable of process `pid`.
pub fn globals_of(pid: pid_t) -> Result<Vec<Global>> {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).context("exe link")?;
    let data = std::fs::read(&exe).with_context(|| format!("read {}", exe.display()))?;
    let elf = goblin::elf::Elf::parse(&data).map_err(|e| anyhow!("parse {}: {e}", exe.display()))?;
    let maps = read_maps(pid)?;
    let exe_s = exe.to_string_lossy().into_owned();
    let first = maps
        .iter()
        .find(|m| m.path == exe_s && m.offset == 0)
        .ok_or_else(|| anyhow!("executable not found in maps"))?;
    let load_vaddr = elf
        .program_headers
        .iter()
        .find(|p| p.p_type == goblin::elf::program_header::PT_LOAD)
        .map(|p| p.p_vaddr)
        .unwrap_or(0);
    let base = first.start.wrapping_sub(load_vaddr);
    let mut out = Vec::new();
    for sym in elf.syms.iter() {
        if sym.st_type() != goblin::elf::sym::STT_OBJECT || sym.st_size == 0 || sym.st_shndx == 0 {
            continue;
        }
        let Some(sh) = elf.section_headers.get(sym.st_shndx) else { continue };
        if sh.sh_flags & u64::from(goblin::elf::section_header::SHF_WRITE) == 0 {
            continue;
        }
        let Some(name) = elf.strtab.get_at(sym.st_name) else { continue };
        if name.starts_with('_') || name.contains('.') || name.is_empty() {
            continue; // toolchain and compiler-internal objects
        }
        out.push(Global { name: name.to_string(), addr: base + sym.st_value, size: sym.st_size });
    }
    out.sort_by_key(|g| g.addr);
    out.dedup_by(|a, b| a.addr == b.addr);
    Ok(out)
}

/// Resolve a `--watch` item: a symbol name, `name+off`, or a hex address.
pub fn resolve(globals: &[Global], item: &str) -> Result<u64> {
    if let Some(h) = item.strip_prefix("0x") {
        return u64::from_str_radix(h, 16).map_err(|e| anyhow!("bad address {item}: {e}"));
    }
    let (name, off) = match item.split_once('+') {
        Some((n, o)) => (n, o.parse::<u64>().map_err(|e| anyhow!("bad offset in {item}: {e}"))?),
        None => (item, 0),
    };
    globals
        .iter()
        .find(|g| g.name == name)
        .map(|g| g.addr + off)
        .ok_or_else(|| anyhow!("no writable global named {name} in the executable"))
}

impl Session {
    /// Account a hit on the slots in `bits`; slots that reached the cap are
    /// disabled on every thread.
    pub fn watch_account(&mut self, tid: pid_t, bits: u8) -> Result<()> {
        let mut changed = false;
        let ltid = self.tasks.get(&tid).map(|t| t.ltid);
        for i in 0..4 {
            if bits & (1 << i) != 0 && i < self.watchpoints.len() {
                self.watch_hits[i] += 1;
                if let Some(l) = ltid {
                    if !self.watch_threads[i].contains(&l) {
                        self.watch_threads[i].push(l);
                    }
                }
                if self.watch_hits[i] == MAX_HITS_PER_SLOT {
                    log::debug!("watchpoint {:#x} hit {} times; disabling it", self.watchpoints[i], MAX_HITS_PER_SLOT);
                    changed = true;
                }
            }
        }
        if changed {
            let keep: Vec<u64> = self
                .watchpoints
                .iter()
                .enumerate()
                .filter(|(i, _)| self.watch_hits[*i] < MAX_HITS_PER_SLOT)
                .map(|(_, a)| *a)
                .collect();
            // Keep slot numbering stable: disabled slots become address 0
            // with their enable bit off (see set_watchpoints).
            self.watch_disabled = (0..4).filter(|i| *i < self.watchpoints.len() && self.watch_hits[*i] >= MAX_HITS_PER_SLOT).map(|i| 1u8 << i).sum();
            let _ = keep;
            self.apply_watchpoints()?;
        }
        Ok(())
    }

    /// Install the session's watchpoints on every live thread.
    pub fn apply_watchpoints(&mut self) -> Result<()> {
        let addrs = self.active_watchpoints();
        for t in self.tasks.values().filter(|t| t.is_alive()) {
            if t.is_ptrace_stopped() {
                set_watchpoints(t.tid, &addrs)?;
            } else {
                // Blocked in the kernel: set when it next stops.
                self.watch_pending.insert(t.tid);
            }
        }
        Ok(())
    }

    /// The watchpoints still enabled, with disabled slots kept at their
    /// index (address 0, which `set_watchpoints` leaves disabled).
    fn active_watchpoints(&self) -> Vec<u64> {
        self.watchpoints
            .iter()
            .enumerate()
            .map(|(i, a)| if self.watch_disabled & (1 << i) != 0 { 0 } else { *a })
            .collect()
    }

    /// Called when a thread is created or stops after being blocked.
    pub fn watch_new_task(&mut self, tid: pid_t) {
        if self.watchpoints.is_empty() {
            return;
        }
        if let Err(e) = set_watchpoints(tid, &self.active_watchpoints()) {
            log::warn!("could not set watchpoints on task {tid}: {e:#}");
        }
    }
}
