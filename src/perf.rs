//! Hardware "ticks" counter: retired conditional branches, following rr's
//! `PerfCounters`.  One counter is attached to each tracee thread.  Ticks are
//! a deterministic measure of user-space progress, which is what lets us
//! stop a thread at exactly the same instruction when replaying a schedule.

use anyhow::{anyhow, bail, Context, Result};
use libc::pid_t;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::ptrace::TIME_SLICE_SIGNAL;

// perf_event constants and layout (the libc crate does not expose them).
pub const PERF_TYPE_HARDWARE: u32 = 0;
pub const PERF_TYPE_RAW: u32 = 4;
pub const PERF_COUNT_HW_INSTRUCTIONS: u64 = 1;
pub const PERF_FLAG_FD_CLOEXEC: u64 = 1 << 3;
const PERF_EVENT_IOC_ENABLE: libc::c_ulong = 0x2400;
const PERF_EVENT_IOC_DISABLE: libc::c_ulong = 0x2401;
const PERF_EVENT_IOC_RESET: libc::c_ulong = 0x2403;
const PERF_EVENT_IOC_PERIOD: libc::c_ulong = 0x4008_2404;
const F_SETSIG: libc::c_int = 10;
const F_SETOWN_EX: libc::c_int = 15;
const F_OWNER_TID: libc::c_int = 0;

#[repr(C)]
struct FOwnerEx {
    type_: libc::c_int,
    pid: pid_t,
}

/// `struct perf_event_attr` (PERF_ATTR_SIZE_VER8 = 136 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PerfEventAttr {
    pub type_: u32,
    pub size: u32,
    pub config: u64,
    pub sample_period: u64,
    pub sample_type: u64,
    pub read_format: u64,
    pub flags: u64,
    pub wakeup_events: u32,
    pub bp_type: u32,
    pub config1: u64,
    pub config2: u64,
    pub branch_sample_type: u64,
    pub sample_regs_user: u64,
    pub sample_stack_user: u32,
    pub clockid: i32,
    pub sample_regs_intr: u64,
    pub aux_watermark: u32,
    pub sample_max_stack: u16,
    pub reserved_2: u16,
    pub aux_sample_size: u32,
    pub reserved_3: u32,
    pub sig_data: u64,
    pub config3: u64,
}

const FLAG_DISABLED: u64 = 1 << 0;
const FLAG_PINNED: u64 = 1 << 2;
const FLAG_EXCLUDE_KERNEL: u64 = 1 << 5;
const FLAG_EXCLUDE_HV: u64 = 1 << 6;
const FLAG_EXCLUDE_GUEST: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TicksSource {
    /// Hardware retired-conditional-branch counter (deterministic).
    Hardware,
    /// `PERF_COUNT_HW_INSTRUCTIONS` fallback (not fully deterministic).
    Instructions,
    /// No counter at all: scheduling only at syscalls.
    None,
}

#[derive(Clone, Debug)]
pub struct PmuConfig {
    pub name: &'static str,
    pub event_type: u32,
    pub config: u64,
    /// Maximum number of ticks the overflow interrupt may be late by.
    pub skid: u64,
    pub source: TicksSource,
}

/// Detect the CPU microarchitecture via cpuid, mirroring rr's table.
pub fn detect_pmu() -> Result<PmuConfig> {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::__cpuid;
        let r0 = __cpuid(0);
        let mut vendor = [0u8; 12];
        vendor[0..4].copy_from_slice(&r0.ebx.to_le_bytes());
        vendor[4..8].copy_from_slice(&r0.edx.to_le_bytes());
        vendor[8..12].copy_from_slice(&r0.ecx.to_le_bytes());
        let r1 = __cpuid(1);
        let cpu_type = r1.eax & 0xF0FF0;
        let ext_family = (r1.eax >> 20) & 0xff;
        let hw = |name, config, skid| PmuConfig {
            name,
            event_type: PERF_TYPE_RAW,
            config,
            skid,
            source: TicksSource::Hardware,
        };
        match &vendor {
            b"AuthenticAMD" => {
                // Zen family (families 0x17, 0x19, 0x1a): RETIRED_CONDITIONAL_BRANCH_INSTRUCTIONS.
                if ext_family >= 8 {
                    let name = match ext_family {
                        8 => "AMD Zen/Zen2",
                        0xa => "AMD Zen3/Zen4",
                        0xb => "AMD Zen5",
                        _ => "AMD Zen (unknown generation)",
                    };
                    // The overflow interrupt usually lands 20-250 ticks late, but
                    // more than 2000 has been seen under load; an overshoot
                    // breaks a replay, a large bound only makes it slower.
                    return Ok(hw(name, 0x5100d1, 10000));
                }
                bail!("unsupported AMD CPU family {ext_family:#x}");
            }
            b"GenuineIntel" => {
                let cfg = match cpu_type {
                    // Nehalem .. Comet Lake: BR_INST_RETIRED.CONDITIONAL
                    0x106a0 | 0x106e0 | 0x206e0 | 0x20650 | 0x206c0 | 0x206f0 | 0x206a0
                    | 0x206d0 | 0x306e0 | 0x306a0 | 0x306c0 | 0x306f0 | 0x40650 | 0x40660
                    | 0x306d0 | 0x40670 | 0x406e0 | 0x50650 | 0x50660 | 0x506e0 | 0x806e0
                    | 0x906e0 | 0xa0660 | 0xa0650 => {
                        hw("Intel (Nehalem..CometLake)", 0x5101c4, 100)
                    }
                    // Ice/Tiger/Rocket Lake
                    0x706e0 | 0x606a0 | 0x606c0 | 0x806c0 | 0x806d0 | 0xa0670 => {
                        hw("Intel (IceLake..RocketLake)", 0x5111c4, 100)
                    }
                    // Alder/Raptor/Meteor Lake and Xeon Sapphire Rapids+
                    0x906a0 | 0x90670 | 0xb0670 | 0xb06a0 | 0xb06f0 | 0xa06a0 | 0x806f0
                    | 0xc06f0 | 0xa06d0 => hw("Intel (AlderLake+)", 0x5111c4, 125),
                    // Lunar / Arrow Lake
                    0xb06d0 | 0xb0650 | 0xc0650 | 0xc0660 => {
                        hw("Intel (Lunar/ArrowLake)", 0x100005111c4, 125)
                    }
                    // Atom cores
                    0x30670 | 0x406c0 | 0x506c0 | 0x506f0 | 0x706a0 | 0x806a0 | 0x906c0 => {
                        hw("Intel Atom", 0x517ec4, 100)
                    }
                    _ => bail!("unknown Intel CPU type {cpu_type:#x}"),
                };
                return Ok(cfg);
            }
            _ => bail!("unknown CPU vendor {:?}", String::from_utf8_lossy(&vendor)),
        }
    }
    #[allow(unreachable_code)]
    {
        bail!("unsupported architecture")
    }
}

pub fn instructions_pmu() -> PmuConfig {
    PmuConfig {
        name: "instructions retired (fallback)",
        event_type: PERF_TYPE_HARDWARE,
        config: PERF_COUNT_HW_INSTRUCTIONS,
        skid: 20000,
        source: TicksSource::Instructions,
    }
}

pub fn no_pmu() -> PmuConfig {
    PmuConfig {
        name: "none",
        event_type: 0,
        config: 0,
        skid: 0,
        source: TicksSource::None,
    }
}

fn perf_event_open(
    attr: &PerfEventAttr,
    pid: pid_t,
    cpu: i32,
    group: i32,
    flags: u64,
) -> Result<OwnedFd> {
    let fd = unsafe {
        libc::syscall(
            libc::SYS_perf_event_open,
            attr as *const PerfEventAttr,
            pid,
            cpu,
            group,
            flags as libc::c_ulong,
        )
    };
    if fd < 0 {
        return Err(anyhow!(
            "perf_event_open failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

fn base_attr(cfg: &PmuConfig) -> PerfEventAttr {
    let mut attr: PerfEventAttr = unsafe { std::mem::zeroed() };
    attr.type_ = cfg.event_type;
    attr.size = size_of::<PerfEventAttr>() as u32;
    attr.config = cfg.config;
    attr.flags = FLAG_EXCLUDE_KERNEL | FLAG_EXCLUDE_GUEST | FLAG_EXCLUDE_HV | FLAG_PINNED;
    attr
}

/// Per-thread ticks counter.
pub struct TicksCounter {
    fd: Option<OwnedFd>,
    /// The event has been enabled (it then stays enabled: a thread that is
    /// stopped or in the kernel retires no user branches, so there is
    /// nothing to switch off, and that saves two ioctls per resume).
    enabled: bool,
    /// Sample period currently programmed.
    period: u64,
    /// Cumulative count at the last `stop`.
    last: u64,
}

/// Effectively "no interrupt".
pub const UNLIMITED_PERIOD: u64 = 1 << 60;

impl TicksCounter {
    /// Open a counter for `tid`.  The thread must exist (it may be running or
    /// stopped).  The counter starts disabled.
    pub fn open(cfg: &PmuConfig, tid: pid_t) -> Result<TicksCounter> {
        if cfg.source == TicksSource::None {
            let _ = tid;
            return Ok(TicksCounter { fd: None, enabled: false, period: UNLIMITED_PERIOD, last: 0 });
        }
        let mut attr = base_attr(cfg);
        attr.sample_period = UNLIMITED_PERIOD;
        attr.flags |= FLAG_DISABLED;
        attr.wakeup_events = 1;
        let fd = perf_event_open(&attr, tid, -1, -1, PERF_FLAG_FD_CLOEXEC)
            .with_context(|| format!("open ticks counter for tid {tid}"))?;
        // Route overflow as TIME_SLICE_SIGNAL to the tracee thread itself.
        unsafe {
            let own = FOwnerEx {
                type_: F_OWNER_TID,
                pid: tid,
            };
            if libc::fcntl(fd.as_raw_fd(), F_SETOWN_EX, &own as *const FOwnerEx) < 0 {
                bail!("F_SETOWN_EX: {}", std::io::Error::last_os_error());
            }
            if libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, libc::O_ASYNC) < 0 {
                bail!("F_SETFL O_ASYNC: {}", std::io::Error::last_os_error());
            }
            if libc::fcntl(fd.as_raw_fd(), F_SETSIG, TIME_SLICE_SIGNAL) < 0 {
                bail!("F_SETSIG: {}", std::io::Error::last_os_error());
            }
        }
        Ok(TicksCounter { fd: Some(fd), enabled: false, period: UNLIMITED_PERIOD, last: 0 })
    }

    /// Start counting from zero; interrupt the thread after `period` ticks
    /// (0 = never).
    pub fn start(&mut self, period: u64) -> Result<()> {
        let Some(fd) = &self.fd else { return Ok(()) };
        let period = if period == 0 {
            UNLIMITED_PERIOD
        } else {
            period.max(1)
        };
        unsafe {
            // A finite budget counts from now: PERF_EVENT_IOC_PERIOD
            // restarts the period (period_left) even when the value is
            // unchanged.  An unlimited one only needs programming once.
            if period != UNLIMITED_PERIOD || self.period != UNLIMITED_PERIOD {
                let p: u64 = period;
                if libc::ioctl(fd.as_raw_fd(), PERF_EVENT_IOC_PERIOD, &p as *const u64) < 0 {
                    bail!("PERF_EVENT_IOC_PERIOD: {}", std::io::Error::last_os_error());
                }
            }
            if !self.enabled {
                libc::ioctl(fd.as_raw_fd(), PERF_EVENT_IOC_ENABLE, 0);
            }
        }
        self.enabled = true;
        self.period = period;
        Ok(())
    }

    /// Return the number of ticks since the previous `stop` (the counter
    /// keeps running; a stopped thread does not count).
    pub fn stop(&mut self) -> Result<u64> {
        if self.fd.is_none() {
            return Ok(0);
        }
        let v = self.read()?;
        let delta = v.wrapping_sub(self.last);
        self.last = v;
        Ok(delta)
    }

    pub fn read(&self) -> Result<u64> {
        let Some(fd) = &self.fd else { return Ok(0) };
        let mut buf = [0u8; 8];
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr() as *mut _, 8) };
        if n != 8 {
            bail!("read ticks counter: {}", std::io::Error::last_os_error());
        }
        Ok(u64::from_ne_bytes(buf))
    }
}

/// Quick self-test similar to rr's `check_working_counters`: count a known
/// number of conditional branches in this process and make sure the result is
/// what we expect.  Returns the number of ticks observed.
pub fn self_test(cfg: &PmuConfig) -> Result<u64> {
    if cfg.source == TicksSource::None {
        return Ok(0);
    }
    let mut attr = base_attr(cfg);
    attr.sample_period = UNLIMITED_PERIOD;
    attr.flags |= FLAG_DISABLED;
    let fd = perf_event_open(&attr, 0, -1, -1, PERF_FLAG_FD_CLOEXEC)?;
    unsafe {
        libc::ioctl(fd.as_raw_fd(), PERF_EVENT_IOC_RESET, 0);
        libc::ioctl(fd.as_raw_fd(), PERF_EVENT_IOC_ENABLE, 0);
    }
    let mut acc: u64 = 0;
    for i in 0..500u64 {
        let v = std::hint::black_box(i);
        if v & 1 == 1 {
            acc = acc.wrapping_add(v);
        }
    }
    std::hint::black_box(acc);
    unsafe {
        libc::ioctl(fd.as_raw_fd(), PERF_EVENT_IOC_DISABLE, 0);
    }
    let mut buf = [0u8; 8];
    let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr() as *mut _, 8) };
    if n != 8 {
        bail!("read self-test counter");
    }
    let ticks = u64::from_ne_bytes(buf);
    if cfg.source == TicksSource::Hardware && ticks < 500 {
        bail!(
            "ticks counter self-test counted only {ticks} conditional branches (expected >= 500)"
        );
    }
    Ok(ticks)
}
