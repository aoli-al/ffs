//! Fork-based checkpoints: clone a whole (stopped) traced process, threads
//! included, into a new process that is also traced.  This follows rr's
//! `Session::copy_state_to` / `Task::os_fork_into` / `Task::os_clone_into`:
//!
//! 1. every thread of the source is brought into a clean stop
//!    (`Session::normalize_all`);
//! 2. the per-thread kernel state that fork does not copy is captured
//!    (registers, FP/AVX state, signal mask, sigaltstack, robust list,
//!    clear_child_tid address, name);
//! 3. one thread injects `clone(CLONE_PARENT|SIGCHLD)` (a fork) producing a
//!    new traced process with a single thread and a copy-on-write copy of
//!    the address space;
//! 4. in the new process, every other thread is recreated by injecting
//!    `clone(CLONE_VM|CLONE_THREAD|...)`, and the captured state is applied
//!    with ptrace;
//! 5. regular-file descriptors are re-opened so that file offsets are not
//!    shared between the copy and the original.
//!
//! The source session is left exactly as it was, so it can continue running
//! or serve as a frozen checkpoint that is forked again later.

use anyhow::{anyhow, bail, Context, Result};
use libc::pid_t;
use std::collections::BTreeMap;

use crate::mem::read_maps;
use crate::ptrace::{self, Regs, WaitStatus};
use crate::remote::{self, Remote};
use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use crate::session::Session;
use crate::task::{Ltid, State, Stop, Task};
use crate::proc::VWait;
use crate::vtime::VBlock;

const CLONE_VM: u64 = 0x100;
const CLONE_FS: u64 = 0x200;
const CLONE_FILES: u64 = 0x400;
const CLONE_SIGHAND: u64 = 0x800;
const CLONE_PARENT: u64 = 0x8000;
const CLONE_THREAD: u64 = 0x10000;
const CLONE_SYSVSEM: u64 = 0x40000;
const CLONE_SETTLS: u64 = 0x80000;
const CLONE_CHILD_CLEARTID: u64 = 0x200000;
const PR_SET_NAME: u64 = 15;
const PR_GET_TID_ADDRESS: u64 = 40;
const SYS_RESTART_SYSCALL: u64 = 219;

/// Everything about a thread that `fork` does not carry over.
#[derive(Clone)]
struct ThreadState {
    ltid: Ltid,
    tid: pid_t,
    regs: Regs,
    xstate: Option<Vec<u8>>,
    sigmask: u64,
    tid_address: u64,
    robust_list: (u64, u64),
    altstack: [u8; 24],
    name: Vec<u8>,
    state: State,
    pending_sig: i32,
    ticks: u64,
    unreported: bool,
    execs: u32,
    vdso_patched: bool,
    tsc_mode: i32,
    rseq: Option<(u64, u64, u64)>,
    vblock: Option<VBlock>,
    vdeadline: Option<(u64, i64)>,
    vwait: Option<VWait>,
    /// Thread-directed signals pending in the kernel (lost by clone).
    pending: u64,
}

fn capture(task: &mut Task) -> Result<ThreadState> {
    // A task at a syscall-entry stop is parked first (syscall aborted and
    // rewound), so that the registers we copy re-execute the syscall.
    task.park_from_entry()?;
    let state = task.state;
    let pending_sig = task.pending_sig;
    let ticks = task.ticks;
    let regs = task.regs()?;
    let xstate = ptrace::get_xstate(task.tid).ok();
    let sigmask = ptrace::get_sigmask(task.tid)?;
    let name = std::fs::read(format!("/proc/{}/comm", task.tid))
        .map(|mut v| {
            while v.last() == Some(&b'\n') {
                v.pop();
            }
            v
        })
        .unwrap_or_default();
    let (tid_address, robust_list, altstack) = {
        let mut r = Remote::new(task)?;
        let out = r.alloc_scratch(8)?;
        r.infallible(
            "prctl(PR_GET_TID_ADDRESS)",
            remote::SYS_PRCTL,
            &[PR_GET_TID_ADDRESS, out],
        )?;
        let tid_address = r.task.mem.read_u64(out)?;
        let head = r.alloc_scratch(8)?;
        let len = r.alloc_scratch(8)?;
        r.infallible(
            "get_robust_list",
            remote::SYS_GET_ROBUST_LIST,
            &[0, head, len],
        )?;
        let robust = (r.task.mem.read_u64(head)?, r.task.mem.read_u64(len)?);
        let ss = r.alloc_scratch(24)?;
        r.infallible("sigaltstack", remote::SYS_SIGALTSTACK, &[0, ss])?;
        let mut alt = [0u8; 24];
        r.task.mem.read(ss, &mut alt)?;
        r.restore()?;
        (tid_address, robust, alt)
    };
    Ok(ThreadState {
        ltid: task.ltid,
        tid: task.tid,
        regs,
        xstate,
        sigmask,
        tid_address,
        robust_list,
        altstack,
        name,
        state,
        pending_sig,
        ticks,
        // A parked copy re-executes its syscall and reports it afresh.
        // A parked copy re-executes its syscall and reports it afresh.
        unreported: task.unreported && !matches!(state, State::Stopped(Stop::Parked)),
        execs: task.execs,
        vdso_patched: task.vdso_patched,
        tsc_mode: task.tsc_mode,
        rseq: task.rseq,
        vblock: task.vblock,
        vdeadline: task.vdeadline,
        vwait: task.vwait,
        pending: ptrace::thread_pending_signals(task.tid) & !(1u64 << (ptrace::TIME_SLICE_SIGNAL - 1)),
    })
}

/// Wait for a freshly created tracee to report its initial stop.
fn wait_initial_stop(tid: pid_t) -> Result<()> {
    match ptrace::wait_tid(tid)? {
        Some(WaitStatus::Event(ptrace::PTRACE_EVENT_STOP))
        | Some(WaitStatus::GroupStop(_))
        | Some(WaitStatus::Signal(libc::SIGSTOP)) => Ok(()),
        Some(WaitStatus::Exited(c)) => bail!("new task {tid} exited ({c}) before its first stop"),
        Some(WaitStatus::Signaled(s)) => {
            bail!("new task {tid} killed by {s} before its first stop")
        }
        None => bail!("new task {tid} vanished"),
        Some(other) => {
            log::warn!("unexpected initial status {other:?} for new task {tid}");
            Ok(())
        }
    }
}

/// Apply captured per-thread state to `task` (a stopped thread of the copy).
fn apply(task: &mut Task, st: &ThreadState, is_fork_child: bool) -> Result<()> {
    if let Some(x) = &st.xstate {
        if let Err(e) = ptrace::set_xstate(task.tid, x) {
            log::warn!("could not restore FP/AVX state of T{}: {e:#}", st.ltid);
        }
    }
    ptrace::set_sigmask(task.tid, st.sigmask)?;
    {
        let mut r = Remote::new(task)?;
        if st.robust_list.0 != 0 {
            r.infallible(
                "set_robust_list",
                remote::SYS_SET_ROBUST_LIST,
                &[st.robust_list.0, st.robust_list.1],
            )?;
        }
        if st.tid_address != 0 {
            r.infallible(
                "set_tid_address",
                remote::SYS_SET_TID_ADDRESS,
                &[st.tid_address],
            )?;
        }
        if !is_fork_child {
            // fork copies the altstack and comm of the forking thread; cloned
            // threads need them set explicitly.
            let ss = r.push_scratch(&st.altstack)?;
            let _ = r.syscall(remote::SYS_SIGALTSTACK, &[ss, 0]);
            if !st.name.is_empty() {
                let mut name = st.name.clone();
                name.truncate(15);
                name.push(0);
                let p = r.push_scratch(&name)?;
                let _ = r.syscall(remote::SYS_PRCTL, &[PR_SET_NAME, p]);
            }
        }
        r.restore_to(st.regs, State::Stopped(Stop::Parked))?;
    }
    task.set_regs(&st.regs)?;
    task.vblock = st.vblock;
    task.vdeadline = st.vdeadline;
    task.vwait = st.vwait;
    task.rewound = st.vdeadline.is_some();
    task.state = match st.state {
        State::Blocked { .. } if st.vblock.is_some() => st.state,
        State::Blocked { .. } => State::Stopped(Stop::Parked),
        s => s,
    };
    task.pending_sig = st.pending_sig;
    task.ticks = st.ticks;
    task.unreported = st.unreported;
    task.execs = st.execs;
    task.vdso_patched = st.vdso_patched;
    task.tsc_mode = st.tsc_mode;
    task.rseq = st.rseq;
    task.in_syscall = false;
    Ok(())
}

/// Resources duplicated for a checkpoint so that copies do not share them
/// with the original: one entry per (shared mapping / pipe / eventfd),
/// shared again between the processes of the same checkpoint.
#[derive(Default)]
pub struct ShareCtx {
    /// (dev, inode, offset, len) -> memfd holding a copy of the mapping.
    memfds: HashMap<(String, u64, u64, u64), OwnedFd>,
    /// pipe inode -> (read end, write end) of the replacement pipe.
    pipes: HashMap<u64, (OwnedFd, OwnedFd)>,
    /// Pipe inodes for which both ends are held inside the checkpointed
    /// process set (only those are duplicated).
    internal_pipes: std::collections::HashSet<u64>,
    /// Socket inodes (of socketpairs) held inside the process set.
    internal_sockets: std::collections::HashSet<u64>,
    /// Socketpair peers (both directions), from the session.
    socket_peers: HashMap<u64, u64>,
    /// socket inode -> replacement socket (ours) for that end.
    sockets: HashMap<u64, OwnedFd>,
    /// Processes of the source session.
    pids: Vec<pid_t>,
    /// Inode pairs of the socketpairs created for the copy (the copy's
    /// session must know them to recreate the pairs again later).
    new_peers: Vec<(u64, u64)>,
}

fn fd_inode(fd: RawFd) -> Option<u64> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } < 0 {
        return None;
    }
    Some(st.st_ino)
}

fn fdinfo(pid: pid_t, fd: i32) -> (u64, u64, HashMap<String, String>) {
    let mut pos = 0u64;
    let mut flags = 0u64;
    let mut extra = HashMap::new();
    if let Ok(info) = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")) {
        for line in info.lines() {
            if let Some(v) = line.strip_prefix("pos:") {
                pos = v.trim().parse().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("flags:") {
                flags = u64::from_str_radix(v.trim(), 8).unwrap_or(0);
            } else if let Some((k, v)) = line.split_once(':') {
                extra.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    (pos, flags, extra)
}

/// Record which pipe inodes have both ends inside the process set, and
/// which socketpair ends are held by it.
pub fn collect_pipe_ends(pids: &[pid_t], ctx: &mut ShareCtx) {
    let mut ends: HashMap<u64, (bool, bool)> = HashMap::new();
    let mut sockets = std::collections::HashSet::new();
    for &pid in pids {
        let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        for e in entries.flatten() {
            let Ok(fd) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
            let Ok(target) = std::fs::read_link(e.path()) else { continue };
            let t = target.to_string_lossy();
            if let Some(ino) = t.strip_prefix("socket:[").and_then(|x| x.strip_suffix(']')) {
                if let Ok(ino) = ino.parse::<u64>() {
                    sockets.insert(ino);
                }
                continue;
            }
            if let Some(ino) = t.strip_prefix("pipe:[").and_then(|x| x.strip_suffix(']')) {
                let Ok(ino) = ino.parse::<u64>() else { continue };
                let (_, flags, _) = fdinfo(pid, fd);
                let e = ends.entry(ino).or_default();
                match (flags & libc::O_ACCMODE as u64) as i32 {
                    libc::O_RDONLY => e.0 = true,
                    libc::O_WRONLY => e.1 = true,
                    _ => {
                        e.0 = true;
                        e.1 = true;
                    }
                }
            }
        }
    }
    ctx.internal_pipes = ends.into_iter().filter(|(_, (r, w))| *r && *w).map(|(i, _)| i).collect();
    // A socketpair is recreated only if both of its ends are in the set.
    ctx.internal_sockets = sockets
        .iter()
        .filter(|i| ctx.socket_peers.get(i).map(|p| sockets.contains(p)).unwrap_or(false))
        .copied()
        .collect();
}

/// Copy the data buffered in `from` (a socket we hold) into `to_peer`
/// (the peer of its replacement) without consuming it.
fn copy_socket_data(from: &OwnedFd, to_peer: &OwnedFd, stream: bool) {
    let mut size: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    unsafe {
        libc::getsockopt(from.as_raw_fd(), libc::SOL_SOCKET, libc::SO_RCVBUF, &mut size as *mut _ as *mut libc::c_void, &mut len);
    }
    let mut buf = vec![0u8; (size.max(65536) as usize) * 2];
    let n = unsafe { libc::recv(from.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len(), libc::MSG_PEEK | libc::MSG_DONTWAIT) };
    log::debug!("socket {}: {n} buffered bytes copied into the checkpoint", from.as_raw_fd());
    if n <= 0 {
        return;
    }
    let n = n as usize;
    if !stream {
        log::warn!("socketpair with buffered datagrams: only the first one is copied into the checkpoint");
    }
    let mut off = 0;
    while off < n {
        let w = unsafe { libc::send(to_peer.as_raw_fd(), buf[off..n].as_ptr() as *const libc::c_void, n - off, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) };
        if w <= 0 {
            log::warn!("could not copy buffered socket data: {}", std::io::Error::last_os_error());
            break;
        }
        off += w as usize;
    }
}

fn sockopt_int(fd: RawFd, opt: libc::c_int) -> libc::c_int {
    let mut v: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    unsafe {
        libc::getsockopt(fd, libc::SOL_SOCKET, opt, &mut v as *mut _ as *mut libc::c_void, &mut len);
    }
    v
}

fn ctx_pids(ctx: &ShareCtx) -> &[pid_t] {
    &ctx.pids
}

fn pidfd_getfd(pid: pid_t, fd: i32) -> Result<OwnedFd> {
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if pidfd < 0 {
        bail!("pidfd_open: {}", std::io::Error::last_os_error());
    }
    let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd as i32) };
    let r = unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd.as_raw_fd(), fd, 0) };
    if r < 0 {
        bail!("pidfd_getfd: {}", std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(r as i32) })
}

/// Make regular-file offsets, internal pipes and eventfds private to the copy.
fn privatize_fds(leader: &mut Task, ctx: &mut ShareCtx) -> Result<()> {
    let pid = leader.tid;
    let entries = std::fs::read_dir(format!("/proc/{pid}/fd")).context("list fds")?;
    enum Kind {
        File { pos: u64 },
        Pipe { ino: u64 },
        EventFd { id: String, count: u64, semaphore: bool },
        Socket { ino: u64 },
    }
    let mut todo: Vec<(i32, u64, Kind)> = Vec::new();
    for e in entries.flatten() {
        let Ok(fd) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
        let Ok(target) = std::fs::read_link(e.path()) else { continue };
        let t = target.to_string_lossy().into_owned();
        let (pos, flags, extra) = fdinfo(pid, fd);
        if flags & (libc::O_PATH as u64) != 0 {
            continue;
        }
        if let Some(ino) = t.strip_prefix("pipe:[").and_then(|x| x.strip_suffix(']')) {
            let Ok(ino) = ino.parse::<u64>() else { continue };
            if ctx.internal_pipes.contains(&ino) {
                todo.push((fd, flags, Kind::Pipe { ino }));
            }
        } else if let Some(ino) = t.strip_prefix("socket:[").and_then(|x| x.strip_suffix(']')) {
            let Ok(ino) = ino.parse::<u64>() else { continue };
            if ctx.internal_sockets.contains(&ino) {
                todo.push((fd, flags, Kind::Socket { ino }));
            }
        } else if t == "anon_inode:[eventfd]" {
            let count = extra.get("eventfd-count").and_then(|v| u64::from_str_radix(v, 16).ok()).unwrap_or(0);
            let semaphore = extra.get("eventfd-semaphore").map(|v| v == "1").unwrap_or(false);
            let id = extra.get("eventfd-id").cloned().unwrap_or_else(|| fd.to_string());
            todo.push((fd, flags, Kind::EventFd { id, count, semaphore }));
        } else if fd > 2 {
            let Ok(md) = std::fs::metadata(e.path()) else { continue };
            if md.file_type().is_file() {
                todo.push((fd, flags, Kind::File { pos }));
            }
        }
    }
    if todo.is_empty() {
        return Ok(());
    }
    // Build replacement pipes in the tracer (copying buffered data with tee).
    for (fd, _flags, kind) in &todo {
        if let Kind::Pipe { ino } = kind {
            if ctx.pipes.contains_key(ino) {
                continue;
            }
            let (_, rflags, _) = fdinfo(pid, *fd);
            if (rflags & libc::O_ACCMODE as u64) as i32 == libc::O_WRONLY {
                continue; // need a read end to tee from; another fd will do
            }
            let old_r = pidfd_getfd(pid, *fd)?;
            let mut np = [0i32; 2];
            if unsafe { libc::pipe2(np.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
                bail!("pipe2: {}", std::io::Error::last_os_error());
            }
            let (nr, nw) = unsafe { (OwnedFd::from_raw_fd(np[0]), OwnedFd::from_raw_fd(np[1])) };
            let size = unsafe { libc::fcntl(old_r.as_raw_fd(), libc::F_GETPIPE_SZ) };
            if size > 0 {
                unsafe { libc::fcntl(nw.as_raw_fd(), libc::F_SETPIPE_SZ, size) };
            }
            // tee does not consume its input, so one call (with a length of
            // at least the pipe capacity) copies everything that is buffered.
            let cap = if size > 0 { size as usize } else { 1 << 20 };
            let n = unsafe { libc::tee(old_r.as_raw_fd(), nw.as_raw_fd(), cap.max(1 << 16), libc::SPLICE_F_NONBLOCK) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::EAGAIN) {
                    log::warn!("could not copy buffered data of pipe {ino}: {e}");
                }
            }
            ctx.pipes.insert(*ino, (nr, nw));
        }
    }
    // Build replacement socketpairs in the tracer, copying buffered data.
    for (fd, _flags, kind) in &todo {
        let Kind::Socket { ino } = kind else { continue };
        if ctx.sockets.contains_key(ino) {
            continue;
        }
        let peer_ino = ctx.socket_peers[ino];
        let old_a = pidfd_getfd(pid, *fd)?;
        let domain = sockopt_int(old_a.as_raw_fd(), libc::SO_DOMAIN);
        let ty = sockopt_int(old_a.as_raw_fd(), libc::SO_TYPE);
        let mut sv = [0i32; 2];
        if unsafe { libc::socketpair(domain, ty | libc::SOCK_CLOEXEC, 0, sv.as_mut_ptr()) } < 0 {
            log::warn!("could not recreate socketpair: {}", std::io::Error::last_os_error());
            continue;
        }
        let (new_a, new_b) = unsafe { (OwnedFd::from_raw_fd(sv[0]), OwnedFd::from_raw_fd(sv[1])) };
        // Data queued at the old end A was sent by B: send it from new B.
        copy_socket_data(&old_a, &new_b, ty == libc::SOCK_STREAM);
        // Find the old peer end in some process of the set to copy the
        // other direction.
        'outer: for p in std::iter::once(&pid).chain(ctx_pids(ctx).iter()) {
            let Ok(entries) = std::fs::read_dir(format!("/proc/{p}/fd")) else { continue };
            for e in entries.flatten() {
                let Ok(pfd) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
                if crate::session::socket_inode(*p, pfd) == Some(peer_ino) {
                    if let Ok(old_b) = pidfd_getfd(*p, pfd) {
                        copy_socket_data(&old_b, &new_a, ty == libc::SOCK_STREAM);
                    }
                    break 'outer;
                }
            }
        }
        if let (Some(ia), Some(ib)) = (fd_inode(new_a.as_raw_fd()), fd_inode(new_b.as_raw_fd())) {
            ctx.new_peers.push((ia, ib));
        }
        ctx.sockets.insert(*ino, new_a);
        ctx.sockets.insert(peer_ino, new_b);
    }
    // Hand the sockets and pipe ends this process needs over with
    // SCM_RIGHTS (its /proc is that of its pid namespace, so it cannot
    // open /proc/<ffs>/fd).
    let wanted: Vec<(i32, u64, RawFd)> = todo
        .iter()
        .filter_map(|(fd, flags, k)| match k {
            Kind::Socket { ino } if ctx.sockets.contains_key(ino) => Some((*fd, *flags, ctx.sockets[ino].as_raw_fd())),
            Kind::Pipe { ino } => {
                let (nr, nw) = ctx.pipes.get(ino)?;
                let end = match (flags & libc::O_ACCMODE as u64) as i32 {
                    libc::O_WRONLY => nw.as_raw_fd(),
                    _ => nr.as_raw_fd(),
                };
                Some((*fd, *flags, end))
            }
            _ => None,
        })
        .collect();
    if !wanted.is_empty() {
        let raw: Vec<RawFd> = wanted.iter().map(|(_, _, end)| *end).collect();
        match crate::fdpass::pass_fds(leader, &raw) {
            Ok(got) => {
                log::debug!("process {pid}: descriptors {:?} received as fds {got:?}", wanted.iter().map(|w| w.0).collect::<Vec<_>>());
                let mut r = Remote::new(leader)?;
                for ((fd, flags, _), newfd) in wanted.iter().zip(got) {
                    let cloexec = flags & (libc::O_CLOEXEC as u64);
                    r.infallible("dup3", remote::SYS_DUP3, &[newfd as u64, *fd as u64, cloexec])?;
                    if flags & (libc::O_NONBLOCK as u64) != 0 {
                        let _ = r.syscall(remote::SYS_FCNTL, &[*fd as u64, libc::F_SETFL as u64, libc::O_NONBLOCK as u64]);
                    }
                    r.infallible("close", remote::SYS_CLOSE, &[newfd as u64])?;
                }
                r.restore()?;
            }
            Err(e) => log::warn!("could not pass descriptors to the checkpoint: {e:#}"),
        }
    }
    let mut eventfds_done: HashMap<String, i64> = HashMap::new();
    let mut r = Remote::new(leader)?;
    for (fd, flags, kind) in &todo {
        let cloexec = flags & (libc::O_CLOEXEC as u64);
        match kind {
            Kind::File { pos } => {
                let path = format!("/proc/self/fd/{fd}\0");
                let p = r.push_scratch(path.as_bytes())?;
                let open_flags = flags & !((libc::O_CREAT | libc::O_EXCL | libc::O_TRUNC | libc::O_NOCTTY | libc::O_CLOEXEC) as u64);
                let newfd = r.syscall(remote::SYS_OPENAT, &[(-100i64) as u64, p, open_flags, 0])?;
                if newfd < 0 {
                    log::warn!("could not re-open fd {fd} in checkpoint: {}", std::io::Error::from_raw_os_error(-newfd as i32));
                    continue;
                }
                r.infallible("lseek", remote::SYS_LSEEK, &[newfd as u64, *pos, libc::SEEK_SET as u64])?;
                r.infallible("dup3", remote::SYS_DUP3, &[newfd as u64, *fd as u64, cloexec])?;
                r.infallible("close", remote::SYS_CLOSE, &[newfd as u64])?;
            }
            Kind::Pipe { .. } | Kind::Socket { .. } => {} // passed above
            Kind::EventFd { id, count, semaphore } => {
                let newfd = match eventfds_done.get(id) {
                    Some(&f) => f,
                    None => {
                        let mut ef = flags & (libc::O_NONBLOCK as u64);
                        if *semaphore {
                            ef |= libc::EFD_SEMAPHORE as u64;
                        }
                        let f = r.syscall(remote::SYS_EVENTFD2, &[*count, ef])?;
                        if f < 0 {
                            log::warn!("could not duplicate eventfd {fd}: {}", std::io::Error::from_raw_os_error(-f as i32));
                            continue;
                        }
                        eventfds_done.insert(id.clone(), f);
                        f
                    }
                };
                r.infallible("dup3", remote::SYS_DUP3, &[newfd as u64, *fd as u64, cloexec])?;
            }
        }
    }
    for (_, f) in eventfds_done {
        let _ = r.syscall(remote::SYS_CLOSE, &[f as u64]);
    }
    r.restore()?;
    Ok(())
}

/// Replace writable MAP_SHARED mappings by private copies (memfds) so that
/// the checkpoint and the original stop sharing memory (rr's EmuFs).
fn privatize_shared_maps(leader: &mut Task, ctx: &mut ShareCtx) -> Result<()> {
    let pid = leader.tid;
    let maps = read_maps(pid)?;
    let todo: Vec<_> = maps
        .into_iter()
        .filter(|m| m.is_shared() && m.is_writable())
        .filter(|m| !matches!(m.path.as_str(), "[vvar]" | "[vdso]" | "[vsyscall]"))
        .filter(|m| !m.path.starts_with("/dev/") || m.path.starts_with("/dev/zero") || m.path.starts_with("/dev/shm/"))
        .collect();
    if todo.is_empty() {
        return Ok(());
    }
    // Copy every mapping into a memfd (one per distinct object).
    let mut keys = Vec::new();
    for m in &todo {
        let len = m.end - m.start;
        let key = (m.dev.clone(), m.inode, m.offset, len);
        if !ctx.memfds.contains_key(&key) {
            let name = b"ffs-shm\0";
            let fd = unsafe { libc::memfd_create(name.as_ptr() as *const libc::c_char, libc::MFD_CLOEXEC) };
            if fd < 0 {
                bail!("memfd_create: {}", std::io::Error::last_os_error());
            }
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            if unsafe { libc::ftruncate(fd.as_raw_fd(), len as i64) } < 0 {
                bail!("ftruncate memfd: {}", std::io::Error::last_os_error());
            }
            let file = std::fs::File::from(fd.try_clone()?);
            use std::os::unix::fs::FileExt;
            let mut off = 0u64;
            let mut buf = vec![0u8; (1 << 20).min(len as usize)];
            let mut ok = true;
            while off < len {
                let n = buf.len().min((len - off) as usize);
                if leader.mem.read(m.start + off, &mut buf[..n]).is_err() {
                    ok = false;
                    break;
                }
                file.write_all_at(&buf[..n], off)?;
                off += n as u64;
            }
            if !ok {
                log::warn!("could not copy shared mapping {:#x}-{:#x} ({}); it stays shared", m.start, m.end, m.path);
                keys.push(None);
                continue;
            }
            ctx.memfds.insert(key.clone(), fd);
        }
        keys.push(Some(key));
    }
    // Hand the memfds to the process and map them over the originals.
    let raw: Vec<RawFd> = keys.iter().flatten().map(|k| ctx.memfds[k].as_raw_fd()).collect();
    let got = crate::fdpass::pass_fds(leader, &raw).context("pass shared-mapping copies")?;
    let mut got = got.into_iter();
    let mut r = Remote::new(leader)?;
    for (m, key) in todo.iter().zip(keys) {
        if key.is_none() {
            continue;
        }
        let Some(fd) = got.next() else { break };
        let len = m.end - m.start;
        let flags = (libc::MAP_SHARED | libc::MAP_FIXED) as u64;
        let addr = r.syscall(remote::SYS_MMAP, &[m.start, len, m.prot() as u64, flags, fd as u64, 0])?;
        if addr != m.start as i64 {
            log::warn!("remapping shared mapping at {:#x} failed ({addr})", m.start);
        }
        r.infallible("close", remote::SYS_CLOSE, &[fd as u64])?;
    }
    r.restore()?;
    Ok(())
}

/// Re-issue the rseq registration of the original thread in its copy
/// (`clone(CLONE_VM)` starts a thread unregistered; a thread that had not
/// registered yet must be left alone or glibc's own registration fails).
fn register_rseq(task: &mut Task, reg: (u64, u64, u64)) {
    let (area, len, sig) = reg;
    match Remote::new(task) {
        Ok(mut r) => {
            let ret = r.syscall(remote::SYS_RSEQ, &[area, len, 0, sig]).unwrap_or(-1);
            if ret < 0 && ret != -(libc::EBUSY as i64) {
                log::warn!("rseq re-registration failed for T{} (area {area:#x} len {len}): {}", r.task.ltid, std::io::Error::from_raw_os_error(-ret as i32));
            }
            let _ = r.restore();
        }
        Err(e) => log::warn!("rseq: {e:#}", ),
    }
}

impl Session {
    /// Bring every live task into a state that can be cloned: blocked tasks
    /// are interrupted and rewound to re-execute their syscall, tasks at a
    /// syscall-entry stop are parked the same way.  This is also what a
    /// replay does at a `Fork` step, so that the original and the copies see
    /// the same events afterwards.
    pub fn normalize_all(&mut self) -> Result<()> {
        self.settle()?;
        let tids: Vec<pid_t> = self
            .tasks
            .values()
            .filter(|t| t.is_alive())
            .map(|t| t.tid)
            .collect();
        for tid in tids {
            let t = self.task_mut(tid)?;
            match t.state {
                // Parked by the tracer: already in a ptrace stop.
                State::Blocked { .. } if t.vblock.is_some() => {}
                State::Blocked { .. } => {
                    ptrace::interrupt(tid)?;
                    t.interrupt_pending = true;
                    let st =
                        ptrace::wait_tid(tid)?.ok_or_else(|| anyhow!("task {tid} vanished"))?;
                    match t.did_wait(st)? {
                        Some(Stop::Parked) => {
                            // Interrupted inside the syscall: rewind so it is
                            // re-executed from scratch when resumed.
                            if !t.rewind_interrupted_syscall()? {
                                // The syscall completed with a real result
                                // right at the interrupt; the kernel reports
                                // that as a plain stop with rax set.
                                t.in_syscall = false;
                                let r = t.regs()?;
                                t.state = State::Stopped(Stop::SyscallExit {
                                    nr: r.orig_rax,
                                    ret: r.rax as i64,
                                });
                                t.unreported = true;
                            }
                        }
                        Some(other) => {
                            // Either the syscall was interrupted (exit stop
                            // with a restart result; the interrupt stop is
                            // still pending) or it completed for real just
                            // before the interrupt took effect.
                            let interrupted = matches!(other, Stop::SyscallExit { ret, .. } if ptrace::is_restart_result(ret as u64));
                            if !interrupted {
                                t.unreported = true;
                            }
                            if t.interrupt_pending {
                                // Consume the pending interrupt stop (no user
                                // code runs before it).
                                let saved = t.state;
                                let saved_sig = std::mem::replace(&mut t.pending_sig, 0);
                                t.resume(crate::ptrace::ResumeHow::Syscall, 0)?;
                                let st2 = ptrace::wait_tid(tid)?
                                    .ok_or_else(|| anyhow!("task {tid} vanished"))?;
                                match t.did_wait(st2)? {
                                    Some(Stop::Parked) => {
                                        if interrupted {
                                            if !t.rewind_interrupted_syscall()? {
                                                log::warn!("task {tid}: expected an interrupted syscall after exit stop {other:?}");
                                            }
                                        } else {
                                            t.state = saved;
                                            t.pending_sig = saved_sig;
                                        }
                                    }
                                    Some(Stop::SyscallEntry { nr }) if interrupted => {
                                        // The kernel restarted the syscall
                                        // without a separate interrupt stop:
                                        // park it from the new entry.  A
                                        // restart_syscall entry stands for
                                        // the original syscall.
                                        t.park_from_entry()?;
                                        if nr == SYS_RESTART_SYSCALL {
                                            if let Stop::SyscallExit { nr: orig, .. } = other {
                                                let mut r = t.regs()?;
                                                r.rax = orig;
                                                r.orig_rax = orig;
                                                t.set_regs(&r)?;
                                            }
                                        }
                                    }
                                    Some(s) => {
                                        log::warn!("task {tid}: unexpected stop {s:?} while consuming interrupt (had {other:?})");
                                        t.unreported = true;
                                    }
                                    None => {}
                                }
                            } else if interrupted {
                                // Shouldn't happen: an interrupted syscall
                                // without a pending interrupt stop.
                                t.rewind_interrupted_syscall()?;
                            }
                        }
                        None => {}
                    }
                }
                State::Stopped(Stop::SyscallEntry { .. }) => {
                    t.park_from_entry()?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// True if every live task is in a state that can be cloned faithfully.
    pub fn can_fork(&self) -> bool {
        self.tasks
            .values()
            .filter(|t| t.is_alive())
            .all(|t| match t.state {
                State::Stopped(Stop::ExitEvent) => false,
                State::Stopped(Stop::Signal { .. }) => t.pending_sig == 0,
                State::Stopped(_) | State::Blocked { .. } => true,
                State::Running => false,
                State::Exited => true,
            })
    }

    /// Create a copy of this session: a new set of traced processes/threads
    /// with identical user-space state.  `self` is left as it was.
    ///
    /// With a pid namespace, a `live` copy gets the canonical thread ids
    /// (`CANONICAL_BASE + ltid`, which are free because the previous live
    /// session has been killed), while a frozen copy gets ids from a
    /// separate range so that it never occupies a canonical one.
    pub fn fork_session(&mut self, live: bool) -> Result<Session> {
        self.normalize_all()?;
        if !self.can_fork() {
            bail!("session cannot be checkpointed in its current state (a thread is exiting or has a signal pending)");
        }
        let mut groups: BTreeMap<pid_t, Vec<pid_t>> = BTreeMap::new();
        for t in self.tasks.values().filter(|t| t.is_alive()) {
            groups.entry(t.tgid).or_default().push(t.tid);
        }
        let mut ctx = ShareCtx::default();
        let pids: Vec<pid_t> = groups.keys().copied().collect();
        ctx.socket_peers = self.socket_peers.iter().map(|(a, b)| (*a, *b)).collect();
        ctx.pids = pids.clone();
        collect_pipe_ends(&pids, &mut ctx);
        let mut new = Session {
            cpu_online: self.cpu_online.clone(),
            tasks: BTreeMap::new(),
            ltids: BTreeMap::new(),
            next_ltid: self.next_ltid,
            pmu: self.pmu.clone(),
            opts: self.opts.clone(),
            vclock: self.vclock.clone(),
            main_tgid: 0,
            zombies: Vec::new(),
            logs: self.logs.clone(),
            vrandom: self.vrandom.clone(),
            replay: false,
            expected_signal: None,
            vtimers: self.vtimers.clone(),
            siginfo_queue: self.siginfo_queue.clone(),
            parent_of: self.parent_of.clone(),
            vzombies: self.vzombies.clone(),
            stopped_procs: self.stopped_procs.clone(),
            jobstops: self.jobstops.clone(),
            jobconts: self.jobconts.clone(),
            pdeathsig: self.pdeathsig.clone(),
            exit_override: BTreeMap::new(),
            exited_procs: self.exited_procs.clone(),
            socket_peers: self.socket_peers.clone(),
            watchpoints: Vec::new(),
            watch_requests: self.watch_requests.clone(),
            watch_pending: Default::default(),
            watch_hits: [0; 4],
            watch_disabled: 0,
            file_snapshots: self.file_snapshots.clone(),
            watch_preempt: false,
            progress: self.progress.clone(),
            created_at: self.created_at.clone(),
            first_output: self.first_output,
            prearmed: std::collections::BTreeSet::new(),
            ports_used: self.ports_used.clone(),
            port_plan: self.port_plan.clone(),
            next_port: self.next_port,
            cap_preempt: false,
        };
        let mut pid_map: Vec<(pid_t, pid_t)> = Vec::new();
        for (tgid, tids) in groups {
            let leader_tid = if tids.contains(&tgid) && self.task(tgid)?.is_ptrace_stopped() {
                tgid
            } else {
                *tids
                    .iter()
                    .find(|&&t| self.task(t).map(|t| t.is_ptrace_stopped()).unwrap_or(false))
                    .ok_or_else(|| anyhow!("no stopped thread in process {tgid}"))?
            };
            // Capture state of every thread.
            let mut states = Vec::new();
            for &tid in &tids {
                let t = self.task_mut(tid)?;
                states.push(capture(t).with_context(|| format!("capture state of T{}", t.ltid))?);
            }
            let leader_state = states.iter().find(|s| s.tid == leader_tid).unwrap().clone();
            let shared_pending = ptrace::shared_pending_signals(leader_tid) & !(1u64 << (ptrace::TIME_SLICE_SIGNAL - 1));

            // Fork the process from the leader.
            let ns = self.opts.ns.clone();
            let steer = |ltid: Ltid| -> Result<()> {
                match &ns {
                    Some(ns) if live => ns.steer_canonical(ltid),
                    _ => Ok(()),
                }
            };
            if let Some(ns) = &ns {
                if !live {
                    ns.steer_frozen()?;
                }
            }
            steer(leader_state.ltid)?;
            let child_pid = {
                let leader = self.task_mut(leader_tid)?;
                let mut r = Remote::new(leader)?;
                let mut ret;
                loop {
                    ret = r.syscall(
                        remote::SYS_CLONE,
                        &[CLONE_PARENT | libc::SIGCHLD as u64, 0, 0, 0, 0],
                    )?;
                    if ret != -(libc::EAGAIN as i64) {
                        break;
                    }
                }
                if ret < 0 {
                    bail!(
                        "remote fork failed: {}",
                        std::io::Error::from_raw_os_error(-ret as i32)
                    );
                }
                let child = r.children.first().copied().unwrap_or(ret as pid_t);
                r.restore()?;
                child
            };
            wait_initial_stop(child_pid)?;
            if live {
                self.check_ns_pid(child_pid, leader_state.ltid);
            }
            let mut child_leader = Task::new(child_pid, child_pid, leader_state.ltid, &self.pmu)?;
            child_leader.state = State::Stopped(Stop::Parked);
            if let Err(e) = privatize_fds(&mut child_leader, &mut ctx) {
                log::warn!("could not privatize file descriptors in checkpoint: {e:#}");
            }
            if let Err(e) = privatize_shared_maps(&mut child_leader, &mut ctx) {
                log::warn!("could not privatize shared mappings in checkpoint: {e:#}");
            }
            apply(&mut child_leader, &leader_state, true).context("apply leader state")?;

            // Recreate the other threads inside the child.
            let mut new_threads = Vec::new();
            for st in states.iter().filter(|s| s.tid != leader_tid) {
                steer(st.ltid)?;
                let new_tid = {
                    let mut r = Remote::new(&mut child_leader)?;
                    let mut flags = CLONE_VM
                        | CLONE_FS
                        | CLONE_FILES
                        | CLONE_SIGHAND
                        | CLONE_THREAD
                        | CLONE_SYSVSEM
                        | CLONE_SETTLS;
                    if st.tid_address != 0 {
                        // Only CLEARTID: the word at the address already
                        // holds what the original had (the memory is a
                        // copy, the tid is canonical).  glibc 2.44 points
                        // clear_child_tid at the thread's join state, and
                        // SETTID would overwrite it with the tid, making a
                        // joiner's futex wait fail with EAGAIN in the copy.
                        flags |= CLONE_CHILD_CLEARTID;
                    }
                    let mut ret;
                    loop {
                        ret = r.syscall(
                            remote::SYS_CLONE,
                            &[flags, 0, 0, st.tid_address, st.regs.fs_base],
                        )?;
                        if ret != -(libc::EAGAIN as i64) {
                            break;
                        }
                    }
                    if ret < 0 {
                        bail!(
                            "remote clone of T{} failed: {}",
                            st.ltid,
                            std::io::Error::from_raw_os_error(-ret as i32)
                        );
                    }
                    let t = r.children.first().copied().unwrap_or(ret as pid_t);
                    r.restore()?;
                    t
                };
                wait_initial_stop(new_tid)?;
                if live {
                    self.check_ns_pid(new_tid, st.ltid);
                }
                let mut nt = Task::new(new_tid, child_pid, st.ltid, &self.pmu)?;
                nt.state = State::Stopped(Stop::Parked);
                if let Some(reg) = st.rseq {
                    register_rseq(&mut nt, reg);
                }
                apply(&mut nt, st, false)
                    .with_context(|| format!("apply state of T{}", st.ltid))?;
                new_threads.push(nt);
            }
            // Re-raise the signals that were pending in the original (the
            // siginfo of the tracer's own signals is in `siginfo_queue`).
            for st in &states {
                let tid = if st.tid == leader_tid {
                    child_leader.tid
                } else {
                    match new_threads.iter().find(|t| t.ltid == st.ltid) {
                        Some(t) => t.tid,
                        None => continue,
                    }
                };
                for sig in 1..=64 {
                    if st.pending & (1u64 << (sig - 1)) != 0 {
                        let _ = ptrace::tgkill(child_pid, tid, sig);
                    }
                }
            }
            for sig in 1..=64 {
                if shared_pending & (1u64 << (sig - 1)) != 0 {
                    unsafe {
                        libc::kill(child_pid, sig);
                    }
                }
            }
            if tgid == self.main_tgid {
                new.main_tgid = child_pid;
            }
            pid_map.push((tgid, child_pid));
            new.ltids.insert(child_leader.ltid, child_leader.tid);
            new.tasks.insert(child_leader.tid, child_leader);
            for nt in new_threads {
                new.ltids.insert(nt.ltid, nt.tid);
                new.tasks.insert(nt.tid, nt);
            }
        }
        // Process groups: a copy must not stay in the original's group
        // (that would keep the original's pid number allocated, and a live
        // copy needs the canonical one), so groups led by a process of the
        // session are recreated among the copies.  Leaders first.
        const SYS_SETPGID: u64 = 109;
        let pgids: Vec<(pid_t, pid_t, Option<pid_t>)> = pid_map
            .iter()
            .map(|&(old, newp)| (old, newp, crate::proc::host_pgid(old)))
            .collect();
        for pass in 0..2 {
            for &(old, newp, pg) in &pgids {
                let Some(pg) = pg else { continue };
                let is_leader = pg == old;
                if (pass == 0) != is_leader {
                    continue;
                }
                let target = if is_leader {
                    0
                } else {
                    match pid_map.iter().find(|(o, _)| *o == pg) {
                        Some(&(_, n)) => n,
                        None => continue, // led by a process outside the session
                    }
                };
                if let Some(t) = new.tasks.get_mut(&newp) {
                    let mut r = Remote::new(t)?;
                    let ret = r.syscall(SYS_SETPGID, &[0, target as u64])?;
                    if ret < 0 {
                        log::warn!("setpgid(0, {target}) in checkpoint copy {newp}: {}", std::io::Error::from_raw_os_error(-ret as i32));
                    }
                    r.restore()?;
                }
            }
        }
        for (a, b) in ctx.new_peers.drain(..) {
            new.socket_peers.insert(a, b);
            new.socket_peers.insert(b, a);
        }
        if new.main_tgid == 0 {
            new.main_tgid = *new
                .tasks
                .keys()
                .next()
                .ok_or_else(|| anyhow!("nothing to checkpoint"))?;
        }
        Ok(new)
    }
}
