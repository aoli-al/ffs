//! The tracee-side API (see include/ffs.h): magic syscall numbers.

pub const FFS_SYS_BASE: u64 = 0x0ff5000;
pub const FFS_SYS_CHECKPOINT: u64 = FFS_SYS_BASE + 1;
pub const FFS_SYS_ASSERT: u64 = FFS_SYS_BASE + 2;
pub const FFS_SYS_YIELD: u64 = FFS_SYS_BASE + 3;
pub const FFS_SYS_LOG: u64 = FFS_SYS_BASE + 4;
pub const FFS_SYS_ACTIVE: u64 = FFS_SYS_BASE + 5;
pub const FFS_SYS_BEGIN: u64 = FFS_SYS_BASE + 6;
pub const FFS_SYS_END: u64 = FFS_SYS_BASE + 7;
pub const FFS_SYS_WATCH: u64 = FFS_SYS_BASE + 8;
pub const FFS_SYS_POINT: u64 = FFS_SYS_BASE + 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MagicCall {
    Checkpoint,
    Assert,
    Yield,
    Log,
    Active,
    Begin,
    End,
    /// (addr, len): watch this memory in every branch (hardware watchpoint).
    Watch,
    /// A scheduling point that is not a yield: the thread does not wait for
    /// anyone, it only marks a place where a switch is interesting.
    Point,
}

pub fn decode(nr: u64) -> Option<MagicCall> {
    Some(match nr {
        FFS_SYS_CHECKPOINT => MagicCall::Checkpoint,
        FFS_SYS_ASSERT => MagicCall::Assert,
        FFS_SYS_YIELD => MagicCall::Yield,
        FFS_SYS_LOG => MagicCall::Log,
        FFS_SYS_ACTIVE => MagicCall::Active,
        FFS_SYS_BEGIN => MagicCall::Begin,
        FFS_SYS_END => MagicCall::End,
        FFS_SYS_WATCH => MagicCall::Watch,
        FFS_SYS_POINT => MagicCall::Point,
        _ => return None,
    })
}

/// Value the tracee sees as the result of a magic call.
pub fn return_value(nr: u64) -> i64 {
    match nr {
        FFS_SYS_ACTIVE => 1,
        _ => 0,
    }
}
