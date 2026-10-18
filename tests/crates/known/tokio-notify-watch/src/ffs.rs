//! Raw ffs magic calls (see include/ffs.h); ENOSYS and harmless natively.
#![allow(dead_code)]
use std::ffi::CString;
const BASE: i64 = 0x0ff5000;

unsafe fn raw(nr: i64, a: i64, b: i64) -> i64 {
    let ret: i64;
    unsafe {
        core::arch::asm!("syscall", inlateout("rax") nr => ret, in("rdi") a, in("rsi") b,
            lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    ret
}
pub fn checkpoint(name: &str) {
    let c = CString::new(name).unwrap();
    unsafe { raw(BASE + 1, c.as_ptr() as i64, 0) };
}
pub fn assert(cond: bool, msg: &str) {
    if !cond {
        eprintln!("[harness] ASSERTION FAILED: {msg}");
    }
    let c = CString::new(msg).unwrap();
    let r = unsafe { raw(BASE + 2, cond as i64, c.as_ptr() as i64) };
    if !cond && r != 0 {
        std::process::exit(3);
    }
}
pub fn end() {
    unsafe { raw(BASE + 7, 0, 0) };
}
pub fn watch(addr: usize, len: usize) {
    unsafe { raw(BASE + 8, addr as i64, len as i64) };
}
