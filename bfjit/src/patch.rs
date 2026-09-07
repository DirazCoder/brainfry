//! In-memory fixup resolution: the JIT counterpart of `bfnative`'s
//! `obj::patch` pass. Same fixup kinds, same encoding rules, same range
//! checks — the only difference is where the addresses come from: instead
//! of a container writer deciding a file layout, the addresses are offsets
//! into the live `mmap`/`VirtualAlloc` region that is about to be executed.
//!
//! The layout mirrored here is the one the object writers use:
//!
//! ```text
//! region: [ code (offset 0, entry stub first) ]
//!         [ rodata (16-byte aligned after code) ]
//!         [ GOT/IAT slots (macOS/Windows only; 8 bytes each, 16-aligned) ]
//! ```
//!
//! Resolving `PatchTarget::Got` to a nonzero address is only meaningful on
//! macOS/Windows hosts (Linux generated code does everything through raw
//! syscalls and contains no GOT fixups at all).

use crate::codegen::{ArmAdrpPair, Fixup, Module, PatchKind, PatchTarget};

/// Where each region of the JIT image landed, in absolute runtime
/// addresses. Same shape as `bfnative::obj::Layout`.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    /// Address of code byte 0 — also the entry point (the entry stub is
    /// emitted first).
    pub code_vaddr: u64,
    /// Address of read-only data byte 0.
    pub rodata_vaddr: u64,
    /// Address of GOT slot 0 (macOS) / IAT slot 0 (Windows). Zero on Linux.
    pub got_vaddr: u64,
}

/// Resolves every fixup in `module` against `layout`, rewriting the code
/// bytes in place. On success the code buffer is final: copy it into the
/// JIT region and it is ready to execute.
///
/// Range violations return an error instead of panicking (the object
/// writers assert; a CLI that can fail before entering generated code
/// should fail with a message, and every one of these would mean a
/// codegen bug, not a user error).
pub fn patch(module: &mut Module, layout: &Layout) -> Result<(), String> {
    let fixups: Vec<Fixup> = module.fixups.clone();
    for fixup in fixups {
        let addr = match fixup.target {
            PatchTarget::Label(i) => {
                let off = *module
                    .labels
                    .get(i as usize)
                    .ok_or_else(|| format!("fixup references unknown label {i}"))?;
                if off == u32::MAX {
                    return Err(format!("fixup references unbound label {i}"));
                }
                layout.code_vaddr + off as u64
            }
            PatchTarget::Str(off) => layout.rodata_vaddr + off as u64,
            PatchTarget::Got(i) => {
                if layout.got_vaddr == 0 {
                    return Err("GOT fixup but this target has no GOT (Linux?)".to_string());
                }
                layout.got_vaddr + 8 * i as u64
            }
        };

        let pos = fixup.pos as usize;
        match fixup.kind {
            PatchKind::X86Rel32 => {
                let field = layout.code_vaddr + pos as u64;
                let rel = addr as i64 - (field + 4) as i64;
                let rel = i32::try_from(rel).map_err(|_| {
                    format!("x86 rel32 branch out of range ({rel} bytes)")
                })?;
                module.code[pos..pos + 4].copy_from_slice(&rel.to_le_bytes());
            }
            PatchKind::ArmB => {
                let pc = layout.code_vaddr + pos as u64;
                let off = addr as i64 - pc as i64;
                check_arm_alignment(off, "b/bl")?;
                if off.unsigned_abs() > 0x7FF_FFFF * 4 {
                    return Err(format!("ARM64 b/bl out of range ({off} bytes)"));
                }
                let imm26 = ((off >> 2) as u32) & 0x03FF_FFFF;
                let word = module.read32(fixup.pos) | imm26;
                module.write32(fixup.pos, word);
            }
            PatchKind::ArmCond => {
                let pc = layout.code_vaddr + pos as u64;
                let off = addr as i64 - pc as i64;
                check_arm_alignment(off, "conditional branch")?;
                if off.unsigned_abs() > 0x7_FFFF * 4 {
                    return Err(format!(
                        "ARM64 conditional branch out of range ({off} bytes) — trampoline bug"
                    ));
                }
                let imm19 = ((off >> 2) as u32) & 0x7_FFFF;
                let word = module.read32(fixup.pos) | (imm19 << 5);
                module.write32(fixup.pos, word);
            }
            PatchKind::ArmAdrp { pair } => {
                let pc = layout.code_vaddr + pos as u64;
                let pc_page = pc & !0xFFF;
                let target_page = addr & !0xFFF;
                let delta_pages = (target_page as i64 - pc_page as i64) >> 12;
                if delta_pages.unsigned_abs() >= 1 << 20 {
                    return Err(format!("ADRP out of range ({delta_pages} pages)"));
                }
                let enc = (delta_pages as u32) & 0x1F_FFFF; // 21-bit two's complement
                let immlo = enc & 0x3;
                let immhi = (enc >> 2) & 0x7_FFFF;
                let word = module.read32(fixup.pos) | (immlo << 29) | (immhi << 5);
                module.write32(fixup.pos, word);

                // Paired LDR/ADD at pos + 4 encodes the low 12 bits. The
                // LDR form requires the GOT/IAT slot to be 8-byte aligned —
                // the layout guarantees it by placing the table 16-aligned.
                let off12 = (addr & 0xFFF) as u32;
                let pair_word = match pair {
                    ArmAdrpPair::Ldr => {
                        if !off12.is_multiple_of(8) {
                            return Err(format!(
                                "GOT/IAT slot at +{off12:#x} is not 8-byte aligned"
                            ));
                        }
                        ((off12 / 8) << 10) | module.read32(fixup.pos + 4)
                    }
                    ArmAdrpPair::Add => (off12 << 10) | module.read32(fixup.pos + 4),
                };
                module.write32(fixup.pos + 4, pair_word);
            }
        }
    }
    Ok(())
}

fn check_arm_alignment(off: i64, what: &str) -> Result<(), String> {
    if off.rem_euclid(4) != 0 {
        return Err(format!("ARM64 {what} misaligned ({off} bytes)"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::{self, Emitter, PatchTarget};
    use crate::target::Target;

    /// A tiny emitted module with one x86 rel32 fixup, patched and decoded
    /// back: the point of the test is that the JIT patcher produces the same
    /// displacement math as the object writers.
    #[test]
    fn x86_rel32_resolves_to_the_label_address() {
        let target = Target::from_name("linux-x86_64").unwrap();
        let mut module = codegen::emit(&[bfformat::Op::Zero], target);
        assert!(!module.fixups.is_empty());

        let layout = Layout {
            code_vaddr: 0x1000,
            rodata_vaddr: 0x2000,
            got_vaddr: 0,
        };
        patch(&mut module, &layout).expect("patch");

        // Every Label fixup must now decode to an address inside the code.
        let code_end = layout.code_vaddr + module.code.len() as u64;
        for f in &module.fixups {
            if let PatchTarget::Label(_) = f.target {
                let field =
                    u32::from_le_bytes(module.code[f.pos as usize..f.pos as usize + 4].try_into().unwrap());
                let dst = (layout.code_vaddr + f.pos as u64 + 4) as i64 + field as i32 as i64;
                assert!(dst >= layout.code_vaddr as i64 && dst < code_end as i64);
            }
        }
    }

    #[test]
    fn arm64_branches_patch_word_aligned_and_in_range() {
        for name in ["linux-aarch64", "macos-aarch64", "windows-aarch64"] {
            let target = Target::from_name(name).unwrap();
            // A loop keeps real bracket fixups alive through optimization.
            let ops = bfc::optimize::optimize(bfc::parser::parse("+[>+<-]").unwrap());
            let mut module = codegen::emit(&ops, target);

            let layout = Layout {
                code_vaddr: 0x7F00_0000_0000, // a plausible mmap address
                rodata_vaddr: 0x7F00_0000_1000,
                got_vaddr: if name != "linux-aarch64" {
                    0x7F00_0000_2000
                } else {
                    0
                },
            };
            patch(&mut module, &layout)
                .unwrap_or_else(|e| panic!("{name}: patch failed: {e}"));

            // Sanity: every 4-byte word is still a decodable branch or data
            // word — here it's enough that the pass didn't error and the
            // code length is a multiple of 4.
            assert_eq!(module.code.len() % 4, 0);
        }
    }

    #[test]
    fn got_fixup_without_a_got_is_an_error() {
        let target = Target::from_name("macos-x86_64").unwrap();
        let ops = bfc::optimize::optimize(bfc::parser::parse("+").unwrap());
        let mut module = codegen::emit(&ops, target);
        assert!(
            module.fixups.iter().any(|f| matches!(f.target, PatchTarget::Got(_))),
            "macOS codegen should contain GOT fixups"
        );
        let layout = Layout {
            code_vaddr: 0x1000,
            rodata_vaddr: 0x2000,
            got_vaddr: 0, // deliberately missing
        };
        assert!(patch(&mut module, &layout).is_err());
    }

    #[test]
    fn unbound_label_is_reported() {
        let mut e = Emitter::new(1);
        e.x86_rel32(PatchTarget::Label(0));
        let mut module = codegen::Module {
            code: e.code,
            labels: e.labels, // label 0 left unbound (u32::MAX)
            fixups: e.fixups,
            rodata: e.rodata,
            spans: e.spans,
        };
        let layout = Layout {
            code_vaddr: 0x1000,
            rodata_vaddr: 0x2000,
            got_vaddr: 0,
        };
        assert!(patch(&mut module, &layout).is_err());
    }
}
