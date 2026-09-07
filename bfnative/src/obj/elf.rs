//! ELF64 writer: emits a static, freestanding ET_EXEC executable directly —
//! not a relocatable object — with raw-syscall runtime code already in the
//! machine-code module. No libc, no PT_INTERP, no dynamic section, no
//! section headers (nothing needs them; the kernel only reads the program
//! header table).
//!
//! Layout (vaddr = 0x400000 + file offset everywhere, so the offset/congruence
//! math is trivially correct):
//!
//! ```text
//! file offset  contents
//!         0    ELF header (64 bytes)
//!        64    program headers: PT_LOAD (RX, whole image), PT_GNU_STACK (RW)
//!       176    machine code (16-byte aligned; the entry stub is first, so
//!              e_entry points here)
//!      ....    read-only data (error messages), 16-byte aligned
//! ```
//!
//! One PT_LOAD covers headers + code + rodata as R+X. There is no writable
//! data segment at all: the tape is an anonymous runtime mapping, and the
//! only mutable state (cell pointer, tape base/end) lives in registers.
//! 0x400000 is a multiple of every page size Linux uses (4 KiB / 16 KiB /
//! 64 KiB), so the fixed load address is congruent with file offset 0 on
//! every kernel configuration, including 64 KiB-page aarch64.

use crate::codegen::Module;
use crate::obj::{Layout, align_up, patch};
use crate::target::Arch;

/// Fixed load address. Below 2 GiB, above the mmap-min-addr floor, page
/// aligned for every supported page size.
const IMAGE_BASE: u64 = 0x400_000;

const PT_LOAD: u32 = 1;
const PT_GNU_STACK: u32 = 0x6474_E551;
const PF_EXECUTE: u32 = 1;
const PF_WRITE: u32 = 2;
const PF_READ: u32 = 4;

const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;

pub fn build(module: &mut Module, arch: Arch) -> (Vec<u8>, Layout) {
    let ehdr_size = 64u64;
    let phnum = 2u64;
    let phdr_size = 56u64;

    let code_off = align_up(ehdr_size + phnum * phdr_size, 16); // 176
    let code_vaddr = IMAGE_BASE + code_off;
    let rodata_off = align_up(code_off + module.code.len() as u64, 16);
    let rodata_vaddr = IMAGE_BASE + rodata_off;
    let total = rodata_off + module.rodata.len() as u64;

    let layout = Layout {
        code_vaddr,
        rodata_vaddr,
        got_vaddr: 0,
    };
    patch(module, &layout);

    let mut out = Vec::with_capacity(total as usize);

    // ---- ELF header ----
    let mut ident = [0u8; 16];
    ident[0] = 0x7F;
    ident[1..4].copy_from_slice(b"ELF");
    ident[4] = 2; // ELFCLASS64
    ident[5] = 1; // ELFDATA2LSB
    ident[6] = 1; // EV_CURRENT
    ident[7] = 1; // ELFOSABI_NONE (SysV)
    out.extend_from_slice(&ident);
    out.extend_from_slice(&ET_EXEC.to_le_bytes());
    out.extend_from_slice(
        &(match arch {
            Arch::X86_64 => EM_X86_64,
            Arch::Aarch64 => EM_AARCH64,
        })
        .to_le_bytes(),
    );
    out.extend_from_slice(&1u32.to_le_bytes()); // e_version
    out.extend_from_slice(&(IMAGE_BASE + code_off).to_le_bytes()); // e_entry
    out.extend_from_slice(&ehdr_size.to_le_bytes()); // e_phoff
    out.extend_from_slice(&0u64.to_le_bytes()); // e_shoff
    out.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    out.extend_from_slice(&(ehdr_size as u16).to_le_bytes()); // e_ehsize
    out.extend_from_slice(&(phdr_size as u16).to_le_bytes()); // e_phentsize
    out.extend_from_slice(&(phnum as u16).to_le_bytes()); // e_phnum
    out.extend_from_slice(&0u16.to_le_bytes()); // e_shentsize
    out.extend_from_slice(&0u16.to_le_bytes()); // e_shnum
    out.extend_from_slice(&0u16.to_le_bytes()); // e_shstrndx

    // ---- program headers ----
    // PT_LOAD: the entire file, R+X, identity-mapped at IMAGE_BASE.
    write_phdr(
        &mut out,
        PT_LOAD,
        PF_READ | PF_EXECUTE,
        0,
        IMAGE_BASE,
        total,
        total,
        0x1000,
    );
    // PT_GNU_STACK: plain RW stack (present mostly to keep kernels from
    // applying legacy READ_IMPLIES_EXEC behavior).
    write_phdr(&mut out, PT_GNU_STACK, PF_READ | PF_WRITE, 0, 0, 0, 0, 0x10);

    debug_assert_eq!(out.len() as u64, code_off);

    // ---- code + rodata ----
    out.extend_from_slice(&module.code);
    while out.len() < rodata_off as usize {
        out.push(0);
    }
    out.extend_from_slice(&module.rodata);

    debug_assert_eq!(out.len() as u64, total);

    (out, layout)
}

// One argument per Elf64_Phdr field, in spec order — kept flat on purpose.
#[allow(clippy::too_many_arguments)]
fn write_phdr(
    out: &mut Vec<u8>,
    p_type: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
    align: u64,
) {
    out.extend_from_slice(&p_type.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&vaddr.to_le_bytes());
    out.extend_from_slice(&vaddr.to_le_bytes()); // p_paddr = p_vaddr
    out.extend_from_slice(&filesz.to_le_bytes());
    out.extend_from_slice(&memsz.to_le_bytes());
    out.extend_from_slice(&align.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::Target;

    fn build_for(ops: &[bfformat::Op], name: &str) -> Vec<u8> {
        let mut m = crate::codegen::emit(ops, Target::from_name(name).unwrap());
        build(&mut m, Target::from_name(name).unwrap().arch).0
    }

    #[test]
    fn header_shape_is_correct_for_both_arches() {
        for (name, machine) in [("linux-x86_64", 62u16), ("linux-aarch64", 183u16)] {
            let bytes = build_for(&[bfformat::Op::Zero], name);
            assert_eq!(&bytes[0..4], b"\x7fELF");
            assert_eq!(bytes[4], 2); // 64-bit
            assert_eq!(bytes[5], 1); // little endian
            assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), ET_EXEC);
            assert_eq!(u16::from_le_bytes([bytes[18], bytes[19]]), machine);
            // e_entry
            let entry = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
            assert_eq!(entry, 0x4000b0);
            // e_phoff == 64, e_phnum == 2
            let phoff = u64::from_le_bytes(bytes[32..40].try_into().unwrap());
            assert_eq!(phoff, 64);
            assert_eq!(u16::from_le_bytes([bytes[56], bytes[57]]), 2);
            // first phdr: PT_LOAD R+X at vaddr 0x400000, offset 0
            let ph = &bytes[64..120];
            assert_eq!(u32::from_le_bytes(ph[0..4].try_into().unwrap()), PT_LOAD);
            assert_eq!(u32::from_le_bytes(ph[4..8].try_into().unwrap()), 5);
            let vaddr = u64::from_le_bytes(ph[16..24].try_into().unwrap());
            assert_eq!(vaddr, 0x400000);
            let filesz = u64::from_le_bytes(ph[32..40].try_into().unwrap());
            assert_eq!(filesz, bytes.len() as u64);
            // congruence: vaddr ≡ offset (mod align) — 0x400000 % 0x1000 == 0
            assert_eq!(vaddr % 0x1000, 0);
            // second phdr: PT_GNU_STACK
            assert_eq!(
                u32::from_le_bytes(bytes[120..124].try_into().unwrap()),
                PT_GNU_STACK
            );
        }
    }

    #[test]
    fn resolved_rel32_branches_stay_in_bounds() {
        // +[>+<-] keeps a real loop through the optimizer.
        let source = "+[>+<-]";
        let ops = bfc::optimize::optimize(bfc::parser::parse(source).unwrap());
        let mut m = crate::codegen::emit(&ops, Target::from_name("linux-x86_64").unwrap());
        let layout = Layout {
            code_vaddr: 0x4000b0,
            rodata_vaddr: 0x500000,
            got_vaddr: 0,
        };
        patch(&mut m, &layout);
        // After patching, every branch fixup's rel32 must decode to a target
        // inside the code region.
        let code_end = layout.code_vaddr + m.code.len() as u64;
        for f in &m.fixups {
            if !matches!(f.target, crate::codegen::PatchTarget::Label(_)) {
                continue;
            }
            let field = u32::from_le_bytes(
                m.code[f.pos as usize..f.pos as usize + 4]
                    .try_into()
                    .unwrap(),
            );
            let target = (layout.code_vaddr + f.pos as u64 + 4) as i64 + field as i32 as i64;
            assert!(
                target >= layout.code_vaddr as i64 && target < code_end as i64,
                "branch to {target:#x} escapes code [{:#x}, {:#x})",
                layout.code_vaddr,
                code_end
            );
        }
    }
}
