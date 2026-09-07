//! Hand-written executable-format writers.
//!
//! Each backend (ELF64, Mach-O, PE/COFF) receives the codegen module,
//! decides where the code, read-only data, and GOT/IAT land in the image,
//! runs the shared [`patch`] pass to resolve every fixup to a final virtual
//! address, and then serializes the file byte by byte. No external
//! assembler, linker, or object-file crate is involved anywhere.

pub mod elf;
pub mod macho;
pub mod pe;

use crate::codegen::{ArmAdrpPair, Fixup, Module, PatchKind, PatchTarget};
use crate::target::{Os, Target};

/// Where each region of the generated image ended up.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    /// Virtual address of code byte 0 (also the entry point: the entry stub
    /// is the first thing every backend emits).
    pub code_vaddr: u64,
    /// Virtual address of read-only data byte 0.
    pub rodata_vaddr: u64,
    /// Virtual address of GOT slot 0 (macOS) / IAT slot 0 (Windows). Zero
    /// on Linux, which has no table.
    pub got_vaddr: u64,
}

/// Patches `module` in place (code bytes get their resolved displacements,
/// which the `--emit-asm` listing then shows) and serializes the complete
/// executable image.
///
/// `image_name` is the output file's basename; the Mach-O writer uses it as
/// the ad-hoc code signature's identifier.
pub fn build(module: &mut Module, target: Target, image_name: &str) -> (Vec<u8>, Layout) {
    match target.os {
        Os::Linux => elf::build(module, target.arch),
        Os::Macos => macho::build(module, target.arch, image_name),
        Os::Windows => pe::build(module, target.arch),
    }
}

/// Resolves every fixup in `module` against `layout`, rewriting the code
/// bytes in place. Branch ranges are validated here — a silent wrong-jump
/// miscompile is far worse than a compile-time panic, so every displacement
/// is range-checked before it is written.
pub(crate) fn patch(module: &mut Module, layout: &Layout) {
    let fixups: Vec<Fixup> = module.fixups.clone();
    for fixup in fixups {
        let addr = match fixup.target {
            PatchTarget::Label(i) => layout.code_vaddr + module.labels[i as usize] as u64,
            PatchTarget::Str(off) => layout.rodata_vaddr + off as u64,
            PatchTarget::Got(i) => {
                assert!(layout.got_vaddr != 0, "GOT fixup but the image has no GOT");
                layout.got_vaddr + 8 * i as u64
            }
        };
        let pos = fixup.pos as usize;
        match fixup.kind {
            PatchKind::X86Rel32 => {
                let field = layout.code_vaddr + pos as u64;
                let rel = addr as i64 - (field + 4) as i64;
                let rel = i32::try_from(rel)
                    .unwrap_or_else(|_| panic!("x86 rel32 branch out of range ({rel} bytes)"));
                module.code[pos..pos + 4].copy_from_slice(&rel.to_le_bytes());
            }
            PatchKind::ArmB => {
                let pc = layout.code_vaddr + pos as u64;
                let off = addr as i64 - pc as i64;
                assert!(off.rem_euclid(4) == 0, "ARM64 branch misaligned");
                assert!(
                    off.unsigned_abs() <= 0x7FF_FFFF * 4,
                    "ARM64 b/bl out of range ({off} bytes)"
                );
                let imm26 = ((off >> 2) as u32) & 0x03FF_FFFF;
                let word = module.read32(fixup.pos) | imm26;
                module.write32(fixup.pos, word);
            }
            PatchKind::ArmCond => {
                let pc = layout.code_vaddr + pos as u64;
                let off = addr as i64 - pc as i64;
                assert!(off.rem_euclid(4) == 0, "ARM64 branch misaligned");
                assert!(
                    off.unsigned_abs() <= 0x7_FFFF * 4,
                    "ARM64 conditional branch out of range ({off} bytes) — trampoline bug"
                );
                let imm19 = ((off >> 2) as u32) & 0x7_FFFF;
                let word = module.read32(fixup.pos) | (imm19 << 5);
                module.write32(fixup.pos, word);
            }
            PatchKind::ArmAdrp { pair } => {
                let pc = layout.code_vaddr + pos as u64;
                let pc_page = pc & !0xFFF;
                let target_page = addr & !0xFFF;
                let delta_pages = (target_page as i64 - pc_page as i64) >> 12;
                assert!(
                    delta_pages.unsigned_abs() < 1 << 20,
                    "ADRP out of range ({delta_pages} pages)"
                );
                let enc = (delta_pages as u32) & 0x1F_FFFF; // 21-bit two's complement
                let immlo = enc & 0x3;
                let immhi = (enc >> 2) & 0x7_FFFF;
                let word = module.read32(fixup.pos) | (immlo << 29) | (immhi << 5);
                module.write32(fixup.pos, word);

                // Paired LDR/ADD at pos + 4 encodes the low 12 bits.
                let off12 = (addr & 0xFFF) as u32;
                let pair_word = match pair {
                    ArmAdrpPair::Ldr => {
                        assert_eq!(off12 % 8, 0, "GOT/IAT slot not 8-byte aligned");
                        ((off12 / 8) << 10) | module.read32(fixup.pos + 4)
                    }
                    ArmAdrpPair::Add => (off12 << 10) | module.read32(fixup.pos + 4),
                };
                module.write32(fixup.pos + 4, pair_word);
            }
        }
    }
}

/// Rounds up to a multiple of `align` (a power of two).
pub(crate) fn align_up(value: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}
