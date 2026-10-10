//! vDSO patching.  To intercept time queries we rewrite the entry points of
//! the vDSO's time functions in the tracee's (private, copy-on-write) mapping
//! into plain syscalls: `mov $nr, %eax; syscall; ret`.  rr achieves the same
//! by replacing the whole vDSO with its own page; patching in place is simpler
//! and does not need a preload library.

use anyhow::{anyhow, Context, Result};
use goblin::elf::Elf;

use crate::mem::{read_maps, Mem};

const PATCHES: &[(&str, u32)] = &[
    ("__vdso_clock_gettime", 228),
    ("clock_gettime", 228),
    ("__vdso_gettimeofday", 96),
    ("gettimeofday", 96),
    ("__vdso_time", 201),
    ("time", 201),
    ("__vdso_clock_getres", 229),
    ("clock_getres", 229),
    ("__vdso_getcpu", 309),
    ("getcpu", 309),
];

/// Patch every known time function in the vDSO of the process whose memory
/// is `mem`.  Returns the number of patched symbols.
pub fn patch_vdso(mem: &Mem) -> Result<usize> {
    let maps = read_maps(mem.pid())?;
    let vdso = maps
        .iter()
        .find(|m| m.path == "[vdso]")
        .ok_or_else(|| anyhow!("no [vdso] mapping"))?;
    let size = (vdso.end - vdso.start) as usize;
    let mut image = vec![0u8; size];
    mem.read(vdso.start, &mut image)
        .context("read vdso image")?;
    let elf = Elf::parse(&image).context("parse vdso ELF")?;
    let mut patched = 0;
    let mut seen = std::collections::HashSet::new();
    for sym in elf.dynsyms.iter() {
        let Some(name) = elf.dynstrtab.get_at(sym.st_name) else {
            continue;
        };
        let Some(&(_, nr)) = PATCHES.iter().find(|(n, _)| *n == name) else {
            continue;
        };
        if sym.st_value == 0 || !seen.insert(sym.st_value) {
            continue;
        }
        // mov $nr, %eax ; syscall ; ret
        let mut code = vec![0xb8];
        code.extend_from_slice(&nr.to_le_bytes());
        code.extend_from_slice(&[0x0f, 0x05, 0xc3]);
        let addr = vdso.start + sym.st_value;
        mem.write(addr, &code)
            .with_context(|| format!("patch {name} at {addr:#x}"))?;
        patched += 1;
    }
    Ok(patched)
}
