//! Handing file descriptors to a tracee.  Pipes and files can be reopened
//! through `/proc/<ffs>/fd/N`, but sockets cannot, so the general way is a
//! Unix socket: ffs listens on an abstract address, the tracee is made to
//! connect to it (injected syscalls) and receive the descriptors with
//! `SCM_RIGHTS`.

use anyhow::{bail, Context, Result};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::OnceLock;

use crate::remote::{self, Remote};
use crate::task::Task;

const SYS_SOCKET: u64 = 41;
const SYS_CONNECT: u64 = 42;
const SYS_RECVMSG: u64 = 47;

struct Listener {
    fd: OwnedFd,
    /// Abstract socket name, without the leading NUL.
    name: Vec<u8>,
}

static LISTENER: OnceLock<Listener> = OnceLock::new();

fn listener() -> Result<&'static Listener> {
    if let Some(l) = LISTENER.get() {
        return Ok(l);
    }
    let name = format!("ffs-fdpass-{}", unsafe { libc::getpid() }).into_bytes();
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        bail!("socket: {}", std::io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as u16;
    for (i, b) in name.iter().enumerate() {
        addr.sun_path[i + 1] = *b as libc::c_char;
    }
    let len = (2 + 1 + name.len()) as libc::socklen_t;
    unsafe {
        if libc::bind(fd.as_raw_fd(), &addr as *const _ as *const libc::sockaddr, len) < 0 {
            bail!("bind abstract socket: {}", std::io::Error::last_os_error());
        }
        if libc::listen(fd.as_raw_fd(), 16) < 0 {
            bail!("listen: {}", std::io::Error::last_os_error());
        }
    }
    let _ = LISTENER.set(Listener { fd, name });
    Ok(LISTENER.get().unwrap())
}

/// Give `fds` to the tracee `task` (stopped); returns the descriptor
/// numbers they got there, in order.
pub fn pass_fds(task: &mut Task, fds: &[RawFd]) -> Result<Vec<i32>> {
    if fds.is_empty() {
        return Ok(Vec::new());
    }
    let l = listener()?;
    let mut r = Remote::new(task)?;
    let sock = r.infallible(
        "socket",
        SYS_SOCKET,
        &[libc::AF_UNIX as u64, (libc::SOCK_STREAM | libc::SOCK_CLOEXEC) as u64, 0],
    )?;
    let mut addr = vec![0u8; 2 + 1 + l.name.len()];
    addr[..2].copy_from_slice(&(libc::AF_UNIX as u16).to_ne_bytes());
    addr[3..].copy_from_slice(&l.name);
    let a = r.push_scratch(&addr)?;
    r.infallible("connect", SYS_CONNECT, &[sock as u64, a, addr.len() as u64])?;
    // Accept on our side and send the descriptors.
    let conn = unsafe { libc::accept4(l.fd.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
    if conn < 0 {
        bail!("accept: {}", std::io::Error::last_os_error());
    }
    let conn = unsafe { OwnedFd::from_raw_fd(conn) };
    let space = unsafe { libc::CMSG_SPACE((fds.len() * 4) as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    let mut data = [0u8; 1];
    let mut iov = libc::iovec { iov_base: data.as_mut_ptr() as *mut libc::c_void, iov_len: 1 };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space;
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN((fds.len() * 4) as u32) as usize;
        std::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, libc::CMSG_DATA(c), fds.len() * 4);
        if libc::sendmsg(conn.as_raw_fd(), &msg, 0) < 0 {
            bail!("sendmsg(SCM_RIGHTS): {}", std::io::Error::last_os_error());
        }
    }
    // The tracee receives them: msghdr, iovec, data byte and control buffer
    // live in its scratch area.
    let cmsg = r.alloc_scratch(space)?;
    let byte = r.alloc_scratch(16)?;
    let mut iov_t = [0u8; 16];
    iov_t[..8].copy_from_slice(&byte.to_ne_bytes());
    iov_t[8..].copy_from_slice(&1u64.to_ne_bytes());
    let iovp = r.push_scratch(&iov_t)?;
    let mut mh = [0u8; 56];
    mh[16..24].copy_from_slice(&iovp.to_ne_bytes());
    mh[24..32].copy_from_slice(&1u64.to_ne_bytes());
    mh[32..40].copy_from_slice(&cmsg.to_ne_bytes());
    mh[40..48].copy_from_slice(&(space as u64).to_ne_bytes());
    let mhp = r.push_scratch(&mh)?;
    let n = r.infallible("recvmsg", SYS_RECVMSG, &[sock as u64, mhp, libc::MSG_CMSG_CLOEXEC as u64])?;
    if n != 1 {
        bail!("recvmsg in tracee returned {n}");
    }
    let mut got = vec![0u8; space];
    r.task.mem.read(cmsg, &mut got).context("read cmsg")?;
    let hdr = unsafe { libc::CMSG_LEN(0) } as usize;
    let out: Vec<i32> = got[hdr..hdr + fds.len() * 4]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| i32::from_ne_bytes(*c))
        .collect();
    r.infallible("close", remote::SYS_CLOSE, &[sock as u64])?;
    r.restore()?;
    if out.iter().any(|&f| f < 0) {
        bail!("tracee received invalid descriptors {out:?}");
    }
    Ok(out)
}
