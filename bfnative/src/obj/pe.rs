//! PE/COFF executable writer (PE32+ / 64-bit) for Windows on x86-64 and
//! ARM64 — a full loadable .exe, not an object file.
//!
//! No CRT is involved: the entry point is our own machine code (the entry
//! stub the codegen backends emit first), and I/O plus memory management go
//! straight through kernel32 — `GetStdHandle`, `WriteFile`, `ReadFile`,
//! `ExitProcess`, `VirtualAlloc` — imported through a real import address
//! table that the Windows loader resolves at startup. Win32 was picked over
//! the C runtime on purpose (this mirrors the old backend's rationale): CRT
//! `read`/`write` export names vary across toolchains and CRT flavors,
//! while kernel32's exports are the one stable surface.
//!
//! The IAT is the first thing in `.idata` (section RVAs are 4 KiB-aligned,
//! slots occupy the first 40 bytes), which keeps the ARM64 backend's
//! `ADRP + LDR #imm12` sequence encodable. The code references everything
//! position-relatively (rip-relative on x86-64, ADRP on ARM64), so the
//! image needs no base relocations and ships with RELOCS_STRIPPED at the
//! fixed preferred base 0x140000000.
//!
//! Layout:
//!
//! ```text
//! file 0x000   DOS header (MZ + e_lfanew)
//!      0x040   PE signature, IMAGE_FILE_HEADER, IMAGE_OPTIONAL_HEADER64
//!      0x148   section table (.text, .idata)
//!      0x200   .text  RX: entry stub, program body, runtime, strings
//!      ....    .idata RW: IAT, import descriptors, INT, hint/names, dll name
//! ```

use crate::codegen::{Module, external_symbols};
use crate::obj::{Layout, align_up, patch};
use crate::target::Arch;

const IMAGE_BASE: u64 = 0x1_4000_0000;
const SECTION_RVA: u64 = 0x1000;
const FILE_ALIGN: u64 = 0x200;
const SECTION_ALIGN: u64 = 0x1000;

const MACHINE_AMD64: u16 = 0x8664;
const MACHINE_ARM64: u16 = 0xAA64;

const PE32_PLUS_MAGIC: u16 = 0x20B;
const SUBSYSTEM_CUI: u16 = 3;

const IMAGE_FILE_EXECUTABLE_IMAGE: u16 = 0x0002;
const IMAGE_FILE_RELOCS_STRIPPED: u16 = 0x0001;
const IMAGE_FILE_LARGE_ADDRESS_AWARE: u16 = 0x0020;

const IMAGE_SCN_CODE: u32 = 0x0000_0020;
const IMAGE_SCN_INITIALIZED_DATA: u32 = 0x0000_0040;
const IMAGE_SCN_EXECUTE: u32 = 0x2000_0000;
const IMAGE_SCN_READ: u32 = 0x4000_0000;
const IMAGE_SCN_WRITE: u32 = 0x8000_0000;

// Data directory indices.
const DIR_IMPORT: usize = 1;
const DIR_IAT: usize = 12;

pub fn build(module: &mut Module, arch: Arch) -> (Vec<u8>, Layout) {
    let symbols = external_symbols(crate::target::Os::Windows);
    debug_assert_eq!(symbols.len(), 5);

    // ---- .text layout ----
    let code_off = 0u64; // entry stub is the first thing emitted
    let rodata_off = align_up(module.code.len() as u64, 16);
    let text_vsize = rodata_off + module.rodata.len() as u64;
    let text_raw = align_up(text_vsize, FILE_ALIGN);
    let text_fileoff = 0x200u64;

    // ---- .idata layout ----
    let idata_rva = SECTION_RVA + align_up(text_vsize, SECTION_ALIGN);
    let iat_off = 0u64; // IAT first (see module docs)
    let descriptor_off = iat_off + 8 * (symbols.len() as u64 + 1);
    let int_off = descriptor_off + 2 * 20;
    let hint_off = int_off + 8 * (symbols.len() as u64 + 1);
    let mut hint_entries: Vec<(String, u64)> = Vec::new(); // (name, offset)
    let mut cursor = hint_off;
    for name in symbols {
        let padded = ((2 + name.len() + 1 + 1) & !1) as u64; // hint u16 + name + NUL, even-padded
        hint_entries.push((name.to_string(), cursor));
        cursor += padded;
    }
    let dll_name_off = cursor;
    let dll_name = b"kernel32.dll";
    let idata_vsize = dll_name_off + dll_name.len() as u64 + 1;
    let idata_raw = align_up(idata_vsize, FILE_ALIGN);
    let idata_fileoff = align_up(text_fileoff + text_raw, FILE_ALIGN);

    // ---- patch code ----
    let layout = Layout {
        code_vaddr: IMAGE_BASE + SECTION_RVA + code_off,
        rodata_vaddr: IMAGE_BASE + SECTION_RVA + rodata_off,
        got_vaddr: IMAGE_BASE + idata_rva,
    };
    patch(module, &layout);

    let size_of_image = idata_rva + align_up(idata_vsize, SECTION_ALIGN);

    // ---- serialize ----
    let mut out: Vec<u8> = Vec::new();

    // DOS header: the loader only checks e_magic and e_lfanew.
    out.extend_from_slice(&[0x4D, 0x5A]); // "MZ"
    out.extend(std::iter::repeat_n(0u8, 0x3C - 2)); // pad to 0x3C
    debug_assert_eq!(out.len(), 0x3C);
    out.extend_from_slice(&0x40u32.to_le_bytes()); // e_lfanew
    debug_assert_eq!(out.len(), 0x40);

    // PE signature
    out.extend_from_slice(b"PE\0\0");

    // IMAGE_FILE_HEADER
    let machine = match arch {
        Arch::X86_64 => MACHINE_AMD64,
        Arch::Aarch64 => MACHINE_ARM64,
    };
    let characteristics = match arch {
        Arch::X86_64 => {
            IMAGE_FILE_EXECUTABLE_IMAGE
                | IMAGE_FILE_RELOCS_STRIPPED
                | IMAGE_FILE_LARGE_ADDRESS_AWARE
        }
        Arch::Aarch64 => IMAGE_FILE_EXECUTABLE_IMAGE | IMAGE_FILE_RELOCS_STRIPPED,
    };
    out.extend_from_slice(&machine.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); // NumberOfSections
    out.extend_from_slice(&0u32.to_le_bytes()); // TimeDateStamp
    out.extend_from_slice(&0u32.to_le_bytes()); // PointerToSymbolTable
    out.extend_from_slice(&0u32.to_le_bytes()); // NumberOfSymbols
    out.extend_from_slice(&240u16.to_le_bytes()); // SizeOfOptionalHeader
    out.extend_from_slice(&characteristics.to_le_bytes());

    // IMAGE_OPTIONAL_HEADER64
    out.extend_from_slice(&PE32_PLUS_MAGIC.to_le_bytes());
    out.push(1); // MajorLinkerVersion
    out.push(0); // MinorLinkerVersion
    out.extend_from_slice(&(text_raw as u32).to_le_bytes()); // SizeOfCode
    out.extend_from_slice(&(idata_raw as u32).to_le_bytes()); // SizeOfInitializedData
    out.extend_from_slice(&0u32.to_le_bytes()); // SizeOfUninitializedData
    out.extend_from_slice(&((SECTION_RVA + code_off) as u32).to_le_bytes()); // entry
    out.extend_from_slice(&(SECTION_RVA as u32).to_le_bytes()); // BaseOfCode
    out.extend_from_slice(&IMAGE_BASE.to_le_bytes());
    out.extend_from_slice(&(SECTION_ALIGN as u32).to_le_bytes());
    out.extend_from_slice(&(FILE_ALIGN as u32).to_le_bytes());
    out.extend_from_slice(&6u16.to_le_bytes()); // MajorOperatingSystemVersion
    out.extend_from_slice(&0u16.to_le_bytes()); // MinorOperatingSystemVersion
    out.extend_from_slice(&0u16.to_le_bytes()); // MajorImageVersion
    out.extend_from_slice(&0u16.to_le_bytes()); // MinorImageVersion
    out.extend_from_slice(&6u16.to_le_bytes()); // MajorSubsystemVersion
    out.extend_from_slice(&0u16.to_le_bytes()); // MinorSubsystemVersion
    out.extend_from_slice(&0u32.to_le_bytes()); // Win32VersionValue
    out.extend_from_slice(&(size_of_image as u32).to_le_bytes());
    out.extend_from_slice(&0x200u32.to_le_bytes()); // SizeOfHeaders
    out.extend_from_slice(&0u32.to_le_bytes()); // CheckSum
    out.extend_from_slice(&SUBSYSTEM_CUI.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // DllCharacteristics: no ASLR
    out.extend_from_slice(&0x0010_0000u64.to_le_bytes()); // SizeOfStackReserve (1 MiB)
    out.extend_from_slice(&0x1000u64.to_le_bytes()); // SizeOfStackCommit
    out.extend_from_slice(&0x0010_0000u64.to_le_bytes()); // SizeOfHeapReserve
    out.extend_from_slice(&0x1000u64.to_le_bytes()); // SizeOfHeapCommit
    out.extend_from_slice(&0u32.to_le_bytes()); // LoaderFlags
    out.extend_from_slice(&16u32.to_le_bytes()); // NumberOfRvaAndSizes

    // Data directories: imports and IAT only.
    let mut dirs = [0u64; 16];
    dirs[DIR_IMPORT] = (idata_rva + descriptor_off) | (40 << 32); // 2 descriptors
    dirs[DIR_IAT] = idata_rva | ((8 * (symbols.len() as u64 + 1)) << 32);
    for dir in dirs {
        out.extend_from_slice(&dir.to_le_bytes());
    }
    debug_assert_eq!(out.len(), 0x148);

    // Section table
    write_section_header(
        &mut out,
        ".text",
        text_vsize,
        SECTION_RVA,
        text_raw,
        text_fileoff,
        IMAGE_SCN_CODE | IMAGE_SCN_EXECUTE | IMAGE_SCN_READ,
    );
    write_section_header(
        &mut out,
        ".idata",
        idata_vsize,
        idata_rva,
        idata_raw,
        idata_fileoff,
        IMAGE_SCN_INITIALIZED_DATA | IMAGE_SCN_READ | IMAGE_SCN_WRITE,
    );
    debug_assert_eq!(out.len(), 0x198);

    // .text contents
    while out.len() < text_fileoff as usize {
        out.push(0);
    }
    out.extend_from_slice(&module.code);
    while (out.len() as u64) < text_fileoff + rodata_off {
        out.push(0);
    }
    out.extend_from_slice(&module.rodata);
    while (out.len() as u64) < text_fileoff + text_raw {
        out.push(0);
    }

    // .idata contents
    debug_assert_eq!(out.len() as u64, idata_fileoff);
    let iat_base = out.len() as u64;
    // IAT: one RVA per import (the loader overwrites these in place) plus
    // the NULL terminator.
    for (name, hint) in &hint_entries {
        let _ = name;
        out.extend_from_slice(&(idata_rva + hint).to_le_bytes());
    }
    out.extend_from_slice(&0u64.to_le_bytes());
    debug_assert_eq!(
        out.len() as u64 - iat_base,
        iat_off + 8 * (symbols.len() as u64 + 1)
    );

    // Import descriptors: kernel32 + the required all-zero terminator.
    out.extend_from_slice(&((idata_rva + int_off) as u32).to_le_bytes()); // OriginalFirstThunk (INT)
    out.extend_from_slice(&0u32.to_le_bytes()); // TimeDateStamp (not bound)
    out.extend_from_slice(&0u32.to_le_bytes()); // ForwarderChain
    out.extend_from_slice(&((idata_rva + dll_name_off) as u32).to_le_bytes()); // Name
    out.extend_from_slice(&(idata_rva as u32).to_le_bytes()); // FirstThunk (IAT)
    out.extend(std::iter::repeat_n(0u8, 20)); // terminator descriptor
    debug_assert_eq!(out.len() as u64 - iat_base, descriptor_off + 40);

    // INT: same RVAs as the IAT (name imports, not ordinals).
    for (_, hint) in &hint_entries {
        out.extend_from_slice(&(idata_rva + hint).to_le_bytes());
    }
    out.extend_from_slice(&0u64.to_le_bytes());

    // Hint/name entries: u16 hint (0) + NUL-terminated name, padded even.
    for (name, _) in &hint_entries {
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        if !out.len().is_multiple_of(2) {
            out.push(0);
        }
    }
    // DLL name.
    out.extend_from_slice(dll_name);
    out.push(0);
    while (out.len() as u64) < idata_fileoff + idata_raw {
        out.push(0);
    }

    (out, layout)
}

fn write_section_header(
    out: &mut Vec<u8>,
    name: &str,
    vsize: u64,
    rva: u64,
    raw_size: u64,
    raw_offset: u64,
    characteristics: u32,
) {
    let mut name_buf = [0u8; 8];
    name_buf[..name.len().min(8)].copy_from_slice(&name.as_bytes()[..name.len().min(8)]);
    out.extend_from_slice(&name_buf);
    out.extend_from_slice(&(vsize as u32).to_le_bytes());
    out.extend_from_slice(&(rva as u32).to_le_bytes());
    out.extend_from_slice(&(raw_size as u32).to_le_bytes());
    out.extend_from_slice(&(raw_offset as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // PointerToRelocations
    out.extend_from_slice(&0u32.to_le_bytes()); // PointerToLinenumbers
    out.extend_from_slice(&0u16.to_le_bytes()); // NumberOfRelocations
    out.extend_from_slice(&0u16.to_le_bytes()); // NumberOfLinenumbers
    out.extend_from_slice(&characteristics.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen;
    use crate::target::Target;

    fn build_for(ops: &[bfformat::Op], name: &str) -> Vec<u8> {
        let target = Target::from_name(name).unwrap();
        let mut m = codegen::emit(ops, target);
        build(&mut m, target.arch).0
    }

    // PE structures in file: PE\0\0 at 0x40, IMAGE_FILE_HEADER at 0x44
    // (machine 0x44, nsections 0x46), optional header at 0x58, data
    // directories at 0xC8 (optional header offset 112), section table at
    // 0x148 (40 bytes each).
    const DIRS_OFF: usize = 0xC8;

    fn header_fields(bytes: &[u8]) -> (u32, u16, u16, u32, u32, u32) {
        (
            u32::from_le_bytes(bytes[0x3C..0x40].try_into().unwrap()), // e_lfanew
            u16::from_le_bytes(bytes[0x44..0x46].try_into().unwrap()), // machine
            u16::from_le_bytes(bytes[0x46..0x48].try_into().unwrap()), // nsections
            u32::from_le_bytes(bytes[0x58 + 16..0x58 + 20].try_into().unwrap()), // entry
            u32::from_le_bytes(bytes[0x58 + 24..0x58 + 28].try_into().unwrap()), // imagebase low
            u32::from_le_bytes(bytes[0x58 + 56..0x58 + 60].try_into().unwrap()), // sizeofimage
        )
    }

    #[test]
    fn headers_are_well_formed_for_both_arches() {
        for (name, machine) in [
            ("windows-x86_64", MACHINE_AMD64),
            ("windows-aarch64", MACHINE_ARM64),
        ] {
            let bytes = build_for(&[bfformat::Op::Zero], name);
            assert_eq!(&bytes[0..2], b"MZ");
            let (lfanew, m, nsects, entry, base_low, size_of_image) = header_fields(&bytes);
            assert_eq!(lfanew, 0x40);
            assert_eq!(m, machine);
            assert_eq!(nsects, 2);
            assert_eq!(&bytes[0x40..0x44], b"PE\0\0");
            assert_eq!(entry, 0x1000);
            assert_eq!(base_low, 0x4000_0000); // low 32 of 0x140000000
            assert!(size_of_image >= 0x2000);
            // optional header magic
            assert_eq!(
                u16::from_le_bytes(bytes[0x58..0x5A].try_into().unwrap()),
                PE32_PLUS_MAGIC
            );
            // subsystem CUI at 0x58+68
            let subsystem = u16::from_le_bytes(bytes[0x58 + 68..0x58 + 70].try_into().unwrap());
            assert_eq!(subsystem, SUBSYSTEM_CUI);
        }
    }

    #[test]
    fn import_table_resolves_five_kernel32_functions() {
        for name in ["windows-x86_64", "windows-aarch64"] {
            let bytes = build_for(&[bfformat::Op::Output, bfformat::Op::Input], name);
            let import_dir = u64::from_le_bytes(
                bytes[DIRS_OFF + DIR_IMPORT * 8..DIRS_OFF + DIR_IMPORT * 8 + 8]
                    .try_into()
                    .unwrap(),
            );
            let (desc_rva, desc_size) = (
                (import_dir & 0xFFFF_FFFF) as usize,
                (import_dir >> 32) as usize,
            );
            assert_eq!(desc_size, 40);
            let to_rva = |rva: u64| -> usize {
                // Map RVA → file offset using the section table.
                for i in 0..2 {
                    let h = 0x148 + 40 * i;
                    let vsize = u32::from_le_bytes(bytes[h + 8..h + 12].try_into().unwrap()) as u64;
                    let rva_sec =
                        u32::from_le_bytes(bytes[h + 12..h + 16].try_into().unwrap()) as u64;
                    let raw = u32::from_le_bytes(bytes[h + 20..h + 24].try_into().unwrap()) as u64;
                    if rva >= rva_sec && rva < rva_sec + vsize {
                        return (raw + (rva - rva_sec)) as usize;
                    }
                }
                panic!("rva {rva:#x} not in any section");
            };

            // Walk the descriptors.
            let mut pos = to_rva(desc_rva as u64);
            let mut descriptors = 0;
            let mut names: Vec<String> = Vec::new();
            loop {
                let oft = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
                let name_rva = u32::from_le_bytes(bytes[pos + 12..pos + 16].try_into().unwrap());
                let ft = u32::from_le_bytes(bytes[pos + 16..pos + 20].try_into().unwrap());
                if oft == 0 && name_rva == 0 && ft == 0 {
                    break;
                }
                descriptors += 1;
                // DLL name
                let dll_pos = to_rva(name_rva as u64);
                let dll_name: String = bytes[dll_pos..]
                    .iter()
                    .take_while(|&&b| b != 0)
                    .map(|&b| b as char)
                    .collect();
                assert_eq!(dll_name, "kernel32.dll");
                // Walk the INT for the imported function names.
                let int_pos = to_rva(oft as u64);
                let mut slot = 0usize;
                loop {
                    let entry = u64::from_le_bytes(
                        bytes[int_pos + 8 * slot..int_pos + 8 * slot + 8]
                            .try_into()
                            .unwrap(),
                    );
                    if entry == 0 {
                        break;
                    }
                    assert_eq!(entry & (1 << 63), 0, "name import, not ordinal");
                    let hint_pos = to_rva(entry);
                    let fn_name: String = bytes[hint_pos + 2..]
                        .iter()
                        .take_while(|&&b| b != 0)
                        .map(|&b| b as char)
                        .collect();
                    names.push(fn_name);
                    slot += 1;
                }
                // FirstThunk must point at the IAT directory.
                let iat_dir = u64::from_le_bytes(
                    bytes[DIRS_OFF + DIR_IAT * 8..DIRS_OFF + DIR_IAT * 8 + 8]
                        .try_into()
                        .unwrap(),
                );
                assert_eq!(ft as u64, iat_dir & 0xFFFF_FFFF);
                pos += 20;
            }
            assert_eq!(descriptors, 1);
            assert_eq!(
                names,
                vec![
                    "GetStdHandle",
                    "WriteFile",
                    "ReadFile",
                    "ExitProcess",
                    "VirtualAlloc"
                ]
            );
        }
    }

    #[test]
    fn iat_is_first_in_idata_so_arm64_ldr_offsets_fit() {
        for name in ["windows-x86_64", "windows-aarch64"] {
            let bytes = build_for(&[bfformat::Op::Output], name);
            let iat_dir = u64::from_le_bytes(
                bytes[DIRS_OFF + DIR_IAT * 8..DIRS_OFF + DIR_IAT * 8 + 8]
                    .try_into()
                    .unwrap(),
            );
            let iat_rva = iat_dir & 0xFFFF_FFFF;
            // .idata section RVA is 0x1000-aligned; the IAT must sit at its
            // very start so ADRP+LDR #imm12 reaches all slots.
            assert_eq!(iat_rva % 0x1000, 0);
            let idata_rva_pos = 0x148 + 40 + 12; // second section header's RVA field
            let idata_rva =
                u32::from_le_bytes(bytes[idata_rva_pos..idata_rva_pos + 4].try_into().unwrap())
                    as u64;
            assert_eq!(iat_rva, idata_rva);
        }
    }
}
