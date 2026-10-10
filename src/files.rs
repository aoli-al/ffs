//! Per-branch isolation of the files a program writes.  Branches of an
//! exploration re-execute the same code, so a file created or modified by
//! one branch would be seen by the next (an "already exists" prompt, an
//! appended log).  Before a syscall that may change a file, the file's
//! current content (or its absence) is remembered; when the branch is over
//! the files are put back.  Files already open for writing when a branch
//! starts are snapshotted then.

use anyhow::Result;
use libc::pid_t;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::session::Session;

const OPEN: u64 = 2;
const RENAME: u64 = 82;
const CREAT: u64 = 85;
const TRUNCATE: u64 = 76;
const UNLINK: u64 = 87;
const OPENAT: u64 = 257;
const UNLINKAT: u64 = 263;
const RENAMEAT: u64 = 264;
const RENAMEAT2: u64 = 316;
const AT_FDCWD: i32 = -100;
/// Files larger than this are left alone (with a warning).
const MAX_SNAPSHOT: u64 = 256 << 20;

/// Content and mode of a file, or `None` if it did not exist.
pub type Snapshot = Option<(Vec<u8>, u32)>;

fn resolve(tid: pid_t, dirfd: i32, path: &str) -> Option<PathBuf> {
    let p = Path::new(path);
    if p.is_absolute() {
        return Some(p.to_path_buf());
    }
    let base = if dirfd == AT_FDCWD {
        std::fs::read_link(format!("/proc/{tid}/cwd")).ok()?
    } else {
        std::fs::read_link(format!("/proc/{tid}/fd/{dirfd}")).ok()?
    };
    Some(base.join(p))
}

fn take_snapshot(path: &Path) -> Result<Snapshot> {
    match std::fs::metadata(path) {
        Ok(md) if md.is_file() => {
            if md.len() > MAX_SNAPSHOT {
                log::warn!("{} is too large to isolate between branches", path.display());
                return Ok(Some((Vec::new(), u32::MAX)));
            }
            Ok(Some((std::fs::read(path)?, md.permissions().mode())))
        }
        Ok(_) => Ok(Some((Vec::new(), u32::MAX))), // not a regular file: untouched
        Err(_) => Ok(None),
    }
}

impl Session {
    fn snapshot(&mut self, path: PathBuf) {
        if self.file_snapshots.contains_key(&path) {
            return;
        }
        match take_snapshot(&path) {
            Ok(s) => {
                self.file_snapshots.insert(path, s);
            }
            Err(e) => log::warn!("cannot snapshot {}: {e}", path.display()),
        }
    }

    /// Before a syscall that may create, change or remove a file.
    pub fn file_pre_syscall(&mut self, tid: pid_t, nr: u64) -> Result<()> {
        if !self.opts.isolate_files
            || !matches!(nr, OPEN | OPENAT | CREAT | TRUNCATE | UNLINK | UNLINKAT | RENAME | RENAMEAT | RENAMEAT2)
        {
            return Ok(());
        }
        let t = self.task_mut(tid)?;
        let r = t.regs()?;
        let mut targets: Vec<(i32, u64)> = Vec::new();
        match nr {
            OPEN | CREAT => {
                let flags = if nr == CREAT { libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC } else { r.rsi as i32 };
                if flags & (libc::O_WRONLY | libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC) != 0 {
                    targets.push((AT_FDCWD, r.rdi));
                }
            }
            OPENAT => {
                let flags = r.rdx as i32;
                if flags & (libc::O_WRONLY | libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC) != 0 {
                    targets.push((r.rdi as i32, r.rsi));
                }
            }
            TRUNCATE | UNLINK => targets.push((AT_FDCWD, r.rdi)),
            UNLINKAT => targets.push((r.rdi as i32, r.rsi)),
            RENAME => {
                targets.push((AT_FDCWD, r.rdi));
                targets.push((AT_FDCWD, r.rsi));
            }
            _ => {
                targets.push((r.rdi as i32, r.rsi));
                targets.push((r.rdx as i32, r.r10));
            }
        }
        let mut paths = Vec::new();
        for (dirfd, ptr) in targets {
            let Ok(p) = t.mem.read_cstr(ptr, 4096) else { continue };
            if p.starts_with("/proc/") || p.starts_with("/dev/") || p.starts_with("/sys/") {
                continue;
            }
            if let Some(full) = resolve(tid, dirfd, &p) {
                paths.push(full);
            }
        }
        for p in paths {
            self.snapshot(p);
        }
        Ok(())
    }

    /// Snapshot the files every process has open for writing (a branch
    /// starts with these inherited from the checkpoint) and refresh the
    /// snapshots inherited from the source session to the current content.
    pub fn files_branch_start(&mut self) {
        if !self.opts.isolate_files {
            return;
        }
        let keys: Vec<PathBuf> = self.file_snapshots.keys().cloned().collect();
        self.file_snapshots.clear();
        for k in keys {
            self.snapshot(k);
        }
        let pids: Vec<pid_t> = self.tasks.values().filter(|t| t.is_alive() && t.tid == t.tgid).map(|t| t.tid).collect();
        for pid in pids {
            let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
            for e in entries.flatten() {
                let Ok(fd) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
                if fd <= 2 {
                    continue;
                }
                let Ok(target) = std::fs::read_link(e.path()) else { continue };
                if !target.is_absolute() || target.starts_with("/proc") || target.starts_with("/dev") {
                    continue;
                }
                let flags = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}"))
                    .ok()
                    .and_then(|s| s.lines().find_map(|l| l.strip_prefix("flags:").map(|v| v.trim().to_string())))
                    .and_then(|v| u32::from_str_radix(&v, 8).ok())
                    .unwrap_or(0);
                if flags & (libc::O_WRONLY | libc::O_RDWR) as u32 != 0 && std::fs::metadata(&target).map(|m| m.is_file()).unwrap_or(false) {
                    self.snapshot(target);
                }
            }
        }
    }

    /// Put every snapshotted file back.
    pub fn restore_files(&mut self) {
        let snaps = std::mem::take(&mut self.file_snapshots);
        for (path, snap) in snaps {
            match snap {
                None => {
                    let _ = std::fs::remove_file(&path);
                }
                Some((_, u32::MAX)) => {}
                Some((data, mode)) => {
                    if let Err(e) = std::fs::write(&path, &data) {
                        log::warn!("cannot restore {}: {e}", path.display());
                        continue;
                    }
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode));
                }
            }
        }
    }
}
