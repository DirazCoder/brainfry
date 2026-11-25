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
//! position-relatively (rip-relative on x86-64, ADRP on ARM64), so nothing
//! in `.text` itself needs fixing up if the loader rebases the image.
//! x86_64 takes advantage of that and ships with RELOCS_STRIPPED and no
//! `.reloc` content at the fixed preferred base 0x140000000. ARM64 can't:
//! the linker rejects disabling ASLR for ARM64/ARM64EC, so it ships one
//! real base relocation (against a dedicated slot in `.idata`, read by
//! nothing at runtime) to satisfy the loader's relocatability check, and a
//! `.pdata`/`.xdata` unwind-info record (the ARM64 loader also rejects an
//! image with stack-allocating code and no exception directory) -- see the
//! comments by `DIR_BASERELOC` and `DIR_EXCEPTION` below for both.
//!
//! Layout (x86_64; ARM64 additionally has a `.pdata` section after
//! `.reloc`):
//!
//! ```text
//! file 0x000   DOS header (MZ + e_lfanew)
//!      0x040   PE signature, IMAGE_FILE_HEADER, IMAGE_OPTIONAL_HEADER64
//!      0x148   section table (.text, .idata, .reloc[, .pdata])
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
const IMAGE_DLLCHARACTERISTICS_HIGH_ENTROPY_VA: u16 = 0x0020;
const IMAGE_DLLCHARACTERISTICS_DYNAMIC_BASE: u16 = 0x0040;
const IMAGE_DLLCHARACTERISTICS_NX_COMPAT: u16 = 0x0100;
const IMAGE_DLLCHARACTERISTICS_TERMINAL_SERVER_AWARE: u16 = 0x8000;

const IMAGE_SCN_CODE: u32 = 0x0000_0020;
const IMAGE_SCN_INITIALIZED_DATA: u32 = 0x0000_0040;
const IMAGE_SCN_EXECUTE: u32 = 0x2000_0000;
const IMAGE_SCN_READ: u32 = 0x4000_0000;
const IMAGE_SCN_WRITE: u32 = 0x8000_0000;

// Data directory indices.
const DIR_IMPORT: usize = 1;
const DIR_EXCEPTION: usize = 3;
const DIR_BASERELOC: usize = 5;
const DIR_IAT: usize = 12;

const IMAGE_SCN_MEM_READ: u32 = 0x4000_0000;
const IMAGE_SCN_MEM_DISCARDABLE: u32 = 0x0200_0000;

// ARM64 Windows requires every function that establishes a stack frame to
// have a corresponding .pdata (RUNTIME_FUNCTION) + .xdata (unwind codes)
// record; the loader validates this (via the Exception Data Directory) and
// rejects images that allocate stack without one with "%1 is not a valid
// Win32 application" -- a CreateProcess-level refusal, confirmed on real
// ARM64 Windows hardware (dumpbin showed an otherwise well-formed image
// with Exception Directory = 0). x86_64 has no equivalent hard requirement
// here, so this is ARM64-only, same pattern as the DYNAMIC_BASE/reloc
// handling above.
//
// The whole of .text (entry stub + translated program + runtime + error
// paths) is one continuous, unreturning stretch with a single fixed-size
// frame that's never torn down -- see emit_entry_windows's "nothing ever
// changes sp again" comment -- so one RUNTIME_FUNCTION covers all of
// .text. Its packed-unwind-data form only has an 11-bit function-length
// field (max ~8KB of code, in 4-byte units), which real generated programs
// can exceed (e.g. mandelbrot), so this uses the full .xdata form instead
// (18-bit function length, effectively no practical ceiling) rather than
// packed unwind data.
//
// Unwind codes: alloc_s (000iiiii, "sub sp,sp,i*16") covers our fixed
// 64-byte (4 * 16) frame, since the dynamic SP realignment that used to
// precede it was removed (Windows ARM64 hardware-enforces 16-byte SP
// alignment at all times, so entry SP is already aligned -- see
// emit_entry_windows), followed by end (0xE4) to terminate the sequence.
// No epilogue scopes: the entry point never executes a normal function
// return (it calls ExitProcess), so there's nothing to describe there.
// Must match the fixed frame size emit_entry_windows allocates on ARM64
// ("sub sp, sp, #64" -- see that function's comment).
const ARM64_ENTRY_FRAME_BYTES: u32 = 64;
const UNWIND_CODE_END: u8 = 0xE4;

const fn alloc_s_unwind_code(frame_bytes: u32) -> u8 {
    assert!(frame_bytes % 16 == 0, "alloc_s requires a 16-byte-aligned frame size");
    let units = frame_bytes / 16;
    assert!(units < 32, "alloc_s's 5-bit immediate covers at most 31*16 = 496 bytes");
    units as u8 // tag bits 000 are implicitly 0 in the low 3 bits of this 5-bit value's byte
}

pub fn build(module: &mut Module, arch: Arch) -> (Vec<u8>, Layout) {
    let symbols = external_symbols(crate::target::Os::Windows);
    debug_assert_eq!(symbols.len(), 6);

    // ---- .text layout ----
    let code_off = 0u64; // entry stub is the first thing emitted
    let rodata_off = align_up(module.code.len() as u64, 16);
    // 4-byte-aligned so ARM64's .xdata Function Length field (.text size in
    // 4-byte units -- see the .pdata content block below) always divides
    // evenly; rodata (error-message strings, NUL-terminated) has no
    // alignment guarantee of its own, so this can't be assumed otherwise.
    // The extra padding (at most 3 bytes) doesn't affect anything else:
    // text_raw and idata_rva round this up again to FILE_ALIGN/SECTION_ALIGN
    // boundaries far coarser than 4 bytes.
    let text_vsize = align_up(rodata_off + module.rodata.len() as u64, 4);
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
    // One extra 8-byte slot after the DLL name, holding a genuine absolute
    // VA (IMAGE_BASE + idata_rva). Nothing reads it at runtime -- its only
    // purpose is to give the base-relocation table a real
    // IMAGE_REL_BASED_DIR64 target instead of the type-0 ABSOLUTE no-op
    // this writer used to emit (see .reloc layout comment below for why
    // that didn't work on real ARM64 Windows hardware).
    let reloc_target_off = align_up(dll_name_off + dll_name.len() as u64 + 1, 8);
    let idata_vsize = reloc_target_off + 8;
    let idata_raw = align_up(idata_vsize, FILE_ALIGN);
    let idata_fileoff = align_up(text_fileoff + text_raw, FILE_ALIGN);

    // ---- .reloc layout ----
    // One relocation block covering the page containing our dedicated
    // reloc-target slot in .idata, with one real IMAGE_REL_BASED_DIR64
    // entry fixing up that slot's absolute VA.
    //
    // This used to be a single IMAGE_REL_BASED_ABSOLUTE (type 0) entry --
    // the standard "no-op but well-formed" padding PE emitters use when
    // there's genuinely nothing to relocate, which is true here: every
    // address baked into .text is PC-relative (ADRP/branch displacements,
    // see obj::patch), so nothing in the *code* needs fixing up if the
    // loader rebases. codesign-equivalent tools (rcodesign, dumpbin
    // /headers) all accepted this as well-formed, and it loads fine on
    // x86_64. But on real ARM64 Windows hardware it doesn't: ASLR can't
    // be disabled for ARM64/ARM64EC (the linker itself rejects
    // /DYNAMICBASE:NO for these targets -- see the characteristics/
    // dll_characteristics comments above), and CreateProcess rejected the
    // image outright with "not a valid Win32 application" -- a
    // loader-level refusal before any code ever ran, confirmed via a raw
    // PE-header parse showing the file was otherwise well-formed
    // (correct machine type, correct PE32+ magic, correct DYNAMIC_BASE
    // bit). The degenerate all-type-0 table apparently doesn't satisfy
    // whatever check the real loader performs to confirm an image is
    // actually relocatable, even though the PE spec doesn't say it has to
    // reject that. Emitting one real, well-formed DIR64 fixup (even
    // against a slot nothing reads) exercises the loader's actual
    // relocation-application path instead of just satisfying a
    // structural well-formedness check.
    //
    // SizeOfBlock must be a multiple of 4 (blocks are walked by adding
    // this field to advance to the next one); one 8-byte header plus one
    // 2-byte entry is 10 bytes, so it's padded with a second, all-zero
    // (type 0) entry to reach 12.
    let reloc_page_rva = idata_rva + (reloc_target_off & !0xFFF);
    let reloc_page_offset = (reloc_target_off & 0xFFF) as u32;
    let reloc_rva = idata_rva + align_up(idata_vsize, SECTION_ALIGN);
    const RELOC_BLOCK_SIZE: u64 = 8 + 2 + 2; // header + one real entry + one zero-pad entry
    let reloc_vsize = RELOC_BLOCK_SIZE;
    let reloc_raw = align_up(reloc_vsize, FILE_ALIGN);
    let reloc_fileoff = align_up(idata_fileoff + idata_raw, FILE_ALIGN);

    // ---- .pdata layout (ARM64 only) ----
    // RUNTIME_FUNCTION (8 bytes: BeginAddress RVA, UnwindData RVA) followed
    // immediately by its .xdata unwind-info blob (two words: header, then
    // one word of unwind codes -- see the ARM64_ENTRY_FRAME_BYTES/alloc_s_unwind_code
    // comment above). Both live in the same section for simplicity; nothing
    // requires them to be split into separate .pdata/.xdata sections.
    let pdata_rva = reloc_rva + align_up(reloc_vsize, SECTION_ALIGN);
    const RUNTIME_FUNCTION_SIZE: u64 = 8;
    const XDATA_SIZE: u64 = 8; // header word + one unwind-code word
    let pdata_vsize = RUNTIME_FUNCTION_SIZE + XDATA_SIZE;
    let pdata_raw = align_up(pdata_vsize, FILE_ALIGN);
    let pdata_fileoff = align_up(reloc_fileoff + reloc_raw, FILE_ALIGN);

    // ---- patch code ----
    let layout = Layout {
        code_vaddr: IMAGE_BASE + SECTION_RVA + code_off,
        rodata_vaddr: IMAGE_BASE + SECTION_RVA + rodata_off,
        got_vaddr: IMAGE_BASE + idata_rva,
    };
    patch(module, &layout);

    let size_of_image = match arch {
        Arch::X86_64 => reloc_rva + align_up(reloc_vsize, SECTION_ALIGN),
        Arch::Aarch64 => pdata_rva + align_up(pdata_vsize, SECTION_ALIGN),
    };


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
    // ARM64 Windows can't disable ASLR (the linker itself rejects
    // /DYNAMICBASE:NO for ARM64/ARM64EC targets), so the ARM64 image
    // stays relocatable: no RELOCS_STRIPPED, DYNAMIC_BASE, and a real
    // base relocation table. x86_64 has no such requirement and a
    // fixed-base, no-relocations image loads there just fine.
    //
    // LARGE_ADDRESS_AWARE is set on both. The image is based at
    // 0x1_4000_0000, above 4 GiB, and lld/MSVC set it on every 64-bit
    // image. It was missing on ARM64 when Windows 11 ARM started
    // rejecting the exe with ERROR_BAD_EXE_FORMAT ("%1 is not a valid
    // Win32 application"); it was the only header-flag difference from
    // lld's output for a minimal ARM64 console exe (along with the
    // DllCharacteristics below). Suspected cause, not bisected on
    // hardware.
    let characteristics = match arch {
        Arch::X86_64 => {
            IMAGE_FILE_EXECUTABLE_IMAGE
                | IMAGE_FILE_RELOCS_STRIPPED
                | IMAGE_FILE_LARGE_ADDRESS_AWARE
        }
        Arch::Aarch64 => IMAGE_FILE_EXECUTABLE_IMAGE | IMAGE_FILE_LARGE_ADDRESS_AWARE,
    };
    let number_of_sections: u16 = match arch {
        Arch::X86_64 => 3,
        Arch::Aarch64 => 4, // + .pdata, required by the ARM64 Windows loader
    };
    out.extend_from_slice(&machine.to_le_bytes());
    out.extend_from_slice(&number_of_sections.to_le_bytes());
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
    let size_of_initialized_data = match arch {
        Arch::X86_64 => idata_raw,
        Arch::Aarch64 => idata_raw + pdata_raw,
    };
    out.extend_from_slice(&(size_of_initialized_data as u32).to_le_bytes()); // SizeOfInitializedData
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
    let dll_characteristics = match arch {
        // Every code address in .text is patched PC-relative (see
        // obj::patch): ADRP/branch displacements, never an absolute VA,
        // so rebasing the image is safe. The set matches what lld emits
        // for ARM64 console executables (0x8160).
        Arch::Aarch64 => {
            IMAGE_DLLCHARACTERISTICS_HIGH_ENTROPY_VA
                | IMAGE_DLLCHARACTERISTICS_DYNAMIC_BASE
                | IMAGE_DLLCHARACTERISTICS_NX_COMPAT
                | IMAGE_DLLCHARACTERISTICS_TERMINAL_SERVER_AWARE
        }
        Arch::X86_64 => 0,
    };
    out.extend_from_slice(&dll_characteristics.to_le_bytes());
    out.extend_from_slice(&0x0010_0000u64.to_le_bytes()); // SizeOfStackReserve (1 MiB)
    out.extend_from_slice(&0x1000u64.to_le_bytes()); // SizeOfStackCommit
    out.extend_from_slice(&0x0010_0000u64.to_le_bytes()); // SizeOfHeapReserve
    out.extend_from_slice(&0x1000u64.to_le_bytes()); // SizeOfHeapCommit
    out.extend_from_slice(&0u32.to_le_bytes()); // LoaderFlags
    out.extend_from_slice(&16u32.to_le_bytes()); // NumberOfRvaAndSizes

    // Data directories: imports, IAT, and base relocations.
    let mut dirs = [0u64; 16];
    dirs[DIR_IMPORT] = (idata_rva + descriptor_off) | (40 << 32); // 2 descriptors
    dirs[DIR_IAT] = idata_rva | ((8 * (symbols.len() as u64 + 1)) << 32);
    dirs[DIR_BASERELOC] = reloc_rva | (RELOC_BLOCK_SIZE << 32);
    if arch == Arch::Aarch64 {
        dirs[DIR_EXCEPTION] = pdata_rva | (RUNTIME_FUNCTION_SIZE << 32);
    }
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
    write_section_header(
        &mut out,
        ".reloc",
        reloc_vsize,
        reloc_rva,
        reloc_raw,
        reloc_fileoff,
        IMAGE_SCN_MEM_READ | IMAGE_SCN_MEM_DISCARDABLE | IMAGE_SCN_INITIALIZED_DATA,
    );
    if arch == Arch::Aarch64 {
        write_section_header(
            &mut out,
            ".pdata",
            pdata_vsize,
            pdata_rva,
            pdata_raw,
            pdata_fileoff,
            IMAGE_SCN_MEM_READ | IMAGE_SCN_INITIALIZED_DATA,
        );
    }
    debug_assert_eq!(out.len() as u64, 0x148 + 40 * number_of_sections as u64);

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
    while (out.len() as u64) < idata_fileoff + reloc_target_off {
        out.push(0);
    }
    // Dedicated reloc-target slot: a genuine absolute VA (nothing reads
    // this at runtime -- see the .reloc layout comment above for why it
    // exists). Written as IMAGE_BASE + idata_rva so its *preferred-base*
    // value is well-defined and checkable; the loader adds the rebase
    // delta on top when the image doesn't load at IMAGE_BASE.
    out.extend_from_slice(&(IMAGE_BASE + idata_rva).to_le_bytes());
    debug_assert_eq!(out.len() as u64, idata_fileoff + reloc_target_off + 8);
    while (out.len() as u64) < idata_fileoff + idata_raw {
        out.push(0);
    }

    // .reloc contents: one block covering the page containing our
    // dedicated reloc-target slot, with one real IMAGE_REL_BASED_DIR64
    // entry. See the .reloc layout comment above for why this replaced
    // the previous type-0 ABSOLUTE no-op entry.
    debug_assert_eq!(out.len() as u64, reloc_fileoff);
    out.extend_from_slice(&(reloc_page_rva as u32).to_le_bytes()); // Page RVA
    out.extend_from_slice(&(RELOC_BLOCK_SIZE as u32).to_le_bytes()); // SizeOfBlock
    const IMAGE_REL_BASED_DIR64: u16 = 10;
    let entry = ((IMAGE_REL_BASED_DIR64 as u16) << 12) | (reloc_page_offset as u16 & 0xFFF);
    out.extend_from_slice(&entry.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // type 0 (ABSOLUTE) pad entry, 4-byte align
    while (out.len() as u64) < reloc_fileoff + reloc_raw {
        out.push(0);
    }

    // .pdata contents (ARM64 only): one RUNTIME_FUNCTION entry covering the
    // whole of .text, pointing at a one-word .xdata unwind-info blob. See
    // the ARM64_ENTRY_FRAME_BYTES/alloc_s_unwind_code/DIR_EXCEPTION comment above for
    // why this exists and why the full .xdata form (not packed unwind
    // data) is used.
    if arch == Arch::Aarch64 {
        debug_assert_eq!(out.len() as u64, pdata_fileoff);
        let xdata_rva = pdata_rva + RUNTIME_FUNCTION_SIZE;
        out.extend_from_slice(&(SECTION_RVA as u32).to_le_bytes()); // BeginAddress
        out.extend_from_slice(&(xdata_rva as u32).to_le_bytes()); // UnwindData -> .xdata

        // .xdata header: Function Length (18 bits, in 4-byte units) | Vers
        // (2 bits, 0) | X (1 bit, 0: no exception handler) | E (1 bit, 0:
        // epilogue scopes follow, but Epilog Count is 0 so none do) |
        // Epilog Count (5 bits, 0) | Code Words (5 bits, 1).
        assert!(
            text_vsize % 4 == 0,
            ".text size must be a multiple of 4 for ARM64 instruction alignment"
        );
        let function_length = text_vsize / 4;
        assert!(
            function_length < (1 << 18),
            ".text exceeds the 18-bit .xdata Function Length field (~1 MiB) -- \
             would need multiple .pdata/.xdata records to describe a program this large"
        );
        let code_words: u32 = 1;
        let epilog_count: u32 = 0;
        let e_bit: u32 = 0;
        let x_bit: u32 = 0;
        let vers: u32 = 0;
        let xdata_header = (code_words << 27)
            | (epilog_count << 22)
            | (e_bit << 21)
            | (x_bit << 20)
            | (vers << 18)
            | (function_length as u32);
        out.extend_from_slice(&xdata_header.to_le_bytes());

        // Unwind codes, byte-stream order, packed into one little-endian
        // word: alloc_s (sub sp,sp,#64) then end. 2 bytes of zero padding
        // fill out the word (padding bytes are unused/ignored).
        let code_bytes = [alloc_s_unwind_code(ARM64_ENTRY_FRAME_BYTES), UNWIND_CODE_END, 0, 0];
        out.extend_from_slice(&code_bytes);
        debug_assert_eq!(out.len() as u64, pdata_fileoff + RUNTIME_FUNCTION_SIZE + XDATA_SIZE);

        while (out.len() as u64) < pdata_fileoff + pdata_raw {
            out.push(0);
        }
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
            // x86_64: .text/.idata/.reloc; ARM64 adds .pdata.
            assert_eq!(nsects, if machine == MACHINE_ARM64 { 4 } else { 3 });
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
    fn windows_images_are_large_address_aware() {
        // The image base is above 4 GiB, so both arches must say so.
        // Missing on ARM64, Windows 11 ARM refuses to start the process
        // (ERROR_BAD_EXE_FORMAT).
        for name in ["windows-x86_64", "windows-aarch64"] {
            let bytes = build_for(&[bfformat::Op::Zero], name);
            let ch = u16::from_le_bytes(bytes[0x56..0x58].try_into().unwrap());
            assert_ne!(ch & IMAGE_FILE_LARGE_ADDRESS_AWARE, 0, "{name}: LAA not set");
        }
    }

    #[test]
    fn arm64_dll_characteristics_match_lld() {
        // Pinned to what lld-link writes for an ARM64 console exe, since
        // that layout is known to load on Windows ARM.
        let bytes = build_for(&[bfformat::Op::Zero], "windows-aarch64");
        let dll_ch = u16::from_le_bytes(bytes[0x58 + 70..0x58 + 72].try_into().unwrap());
        assert_eq!(dll_ch, 0x8160);
        let ch = u16::from_le_bytes(bytes[0x56..0x58].try_into().unwrap());
        assert_eq!(ch & IMAGE_FILE_RELOCS_STRIPPED, 0, "ARM64 must stay relocatable");
    }

    #[test]
    fn import_table_resolves_six_kernel32_functions() {
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
                // Map RVA → file offset using the section table. Written as
                // find_map + expect so there is no unexercised panic branch
                // of our own in the file: the only way to hit the expect is
                // this test misparsing a PE it just built.
                (0..3)
                    .find_map(|i| {
                        let h = 0x148 + 40 * i;
                        let vsize =
                            u32::from_le_bytes(bytes[h + 8..h + 12].try_into().unwrap()) as u64;
                        let rva_sec =
                            u32::from_le_bytes(bytes[h + 12..h + 16].try_into().unwrap()) as u64;
                        let raw =
                            u32::from_le_bytes(bytes[h + 20..h + 24].try_into().unwrap()) as u64;
                        (rva >= rva_sec && rva < rva_sec + vsize)
                            .then(|| (raw + (rva - rva_sec)) as usize)
                    })
                    .expect(&format!("rva {rva:#x} not in any section"))
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
                    "VirtualAlloc",
                    "GetLastError"
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

    #[test]
    fn base_relocation_table_has_a_real_dir64_fixup() {
        // Regression test: this table used to contain a single
        // IMAGE_REL_BASED_ABSOLUTE (type 0) no-op entry. That's spec-legal
        // (dumpbin, rcodesign, and every static PE parser accepted it) and
        // loads fine on x86_64, but real ARM64 Windows hardware rejected
        // the resulting image outright with "not a valid Win32
        // application" -- a CreateProcess-level refusal before any code
        // ran. This test walks the actual relocation block and requires
        // at least one non-ABSOLUTE entry, so a future "simplify this
        // back to a no-op" edit fails loudly instead of shipping a build
        // that only breaks on hardware CI doesn't otherwise exercise.
        const IMAGE_REL_BASED_DIR64: u16 = 10;
        for name in ["windows-x86_64", "windows-aarch64"] {
            let bytes = build_for(&[bfformat::Op::Output], name);

            let reloc_dir = u64::from_le_bytes(
                bytes[DIRS_OFF + DIR_BASERELOC * 8..DIRS_OFF + DIR_BASERELOC * 8 + 8]
                    .try_into()
                    .unwrap(),
            );
            let (reloc_rva, reloc_size) = (
                (reloc_dir & 0xFFFF_FFFF) as u32,
                (reloc_dir >> 32) as u32,
            );
            assert!(reloc_size >= 12, "block must hold header + at least one real entry");

            // Map reloc_rva -> file offset via the section table (3
            // sections: .text, .idata, .reloc).
            let mut reloc_file_off = None;
            for i in 0..3 {
                let h = 0x148 + 40 * i;
                let vsize = u32::from_le_bytes(bytes[h + 8..h + 12].try_into().unwrap()) as u64;
                let rva = u32::from_le_bytes(bytes[h + 12..h + 16].try_into().unwrap()) as u64;
                let raw = u32::from_le_bytes(bytes[h + 20..h + 24].try_into().unwrap()) as u64;
                if reloc_rva as u64 >= rva && (reloc_rva as u64) < rva + vsize.max(1) {
                    reloc_file_off = Some(raw + (reloc_rva as u64 - rva));
                }
            }
            let pos = reloc_file_off.expect("reloc dir RVA maps into some section") as usize;

            let page_rva = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
            let size_of_block = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap());
            assert_eq!(size_of_block % 4, 0, "SizeOfBlock must be 4-byte aligned to be walkable");
            assert!(size_of_block >= 10, "header (8) + at least one entry (2)");

            let mut entries = Vec::new();
            let mut off = pos + 8;
            while off < pos + size_of_block as usize {
                let raw = u16::from_le_bytes(bytes[off..off + 2].try_into().unwrap());
                entries.push((raw >> 12, raw & 0xFFF)); // (type, page-relative offset)
                off += 2;
            }

            let real_entries: Vec<_> = entries.iter().filter(|(t, _)| *t != 0).collect();
            assert!(
                !real_entries.is_empty(),
                "relocation block for {name} has no non-ABSOLUTE entries -- this is the \
                 exact degenerate table that real ARM64 Windows hardware rejected with \
                 \"not a valid Win32 application\""
            );
            for (t, page_off) in &real_entries {
                assert_eq!(*t, IMAGE_REL_BASED_DIR64, "expected a DIR64 fixup");
                let target_rva = page_rva + *page_off as u32;
                assert!(
                    (target_rva as u64) < idata_end(&bytes),
                    "DIR64 fixup at rva {target_rva:#x} must point inside .idata"
                );
            }
        }
    }

    /// End RVA of the .idata section (start + virtual size), for
    /// sanity-checking that a relocation target actually lands inside it.
    fn idata_end(bytes: &[u8]) -> u64 {
        let h = 0x148 + 40; // second section header (.idata)
        let vsize = u32::from_le_bytes(bytes[h + 8..h + 12].try_into().unwrap()) as u64;
        let rva = u32::from_le_bytes(bytes[h + 12..h + 16].try_into().unwrap()) as u64;
        rva + vsize
    }

    #[test]
    fn x86_64_has_no_exception_directory() {
        // x86_64 has no equivalent hard requirement for unwind info on a
        // simple image like this (see the module doc comment), so it
        // shouldn't carry a .pdata section or a populated Exception
        // Directory -- regression guard against accidentally enabling this
        // ARM64-only path for x86_64 too.
        let bytes = build_for(&[bfformat::Op::Output], "windows-x86_64");
        let nsections = u16::from_le_bytes(bytes[0x46..0x48].try_into().unwrap());
        assert_eq!(nsections, 3, "x86_64 should still be .text/.idata/.reloc only");
        let exc_dir = u64::from_le_bytes(
            bytes[DIRS_OFF + DIR_EXCEPTION * 8..DIRS_OFF + DIR_EXCEPTION * 8 + 8]
                .try_into()
                .unwrap(),
        );
        assert_eq!(exc_dir, 0, "x86_64 Exception Directory should be empty");
    }

    #[test]
    fn arm64_exception_directory_describes_the_whole_of_text() {
        // Regression test for "%1 is not a valid Win32 application" on real
        // ARM64 Windows hardware: dumpbin showed an otherwise well-formed
        // image (correct machine type, PE32+ magic, non-degenerate reloc
        // table) rejected at the loader level because its Exception
        // Directory was empty, even though its entry stub allocates a
        // stack frame ("sub sp, sp, #64"). This walks the actual
        // RUNTIME_FUNCTION + .xdata bytes this writer produces and checks
        // they're self-consistent and cover .text, so a future edit that
        // silently drops this (e.g. "simplify, x86_64 doesn't need this")
        // fails loudly instead of only breaking on hardware CI doesn't
        // otherwise exercise.
        let bytes = build_for(&[bfformat::Op::Output], "windows-aarch64");

        let nsections = u16::from_le_bytes(bytes[0x46..0x48].try_into().unwrap());
        assert_eq!(nsections, 4, "ARM64 should have .text/.idata/.reloc/.pdata");

        let exc_dir = u64::from_le_bytes(
            bytes[DIRS_OFF + DIR_EXCEPTION * 8..DIRS_OFF + DIR_EXCEPTION * 8 + 8]
                .try_into()
                .unwrap(),
        );
        let (pdata_rva, pdata_size) = ((exc_dir & 0xFFFF_FFFF) as u32, (exc_dir >> 32) as u32);
        assert_eq!(pdata_size, 8, "exactly one 8-byte RUNTIME_FUNCTION entry");
        assert_ne!(pdata_rva, 0, "Exception Directory must not be empty on ARM64");

        // Map pdata_rva -> file offset via the section table (4th section
        // on ARM64: .text, .idata, .reloc, .pdata).
        let mut pdata_file_off = None;
        let mut text_rva = None;
        let mut text_vsize = None;
        for i in 0..4u64 {
            let h = 0x148 + 40 * i as usize;
            let mut name = [0u8; 8];
            name.copy_from_slice(&bytes[h..h + 8]);
            let vsize = u32::from_le_bytes(bytes[h + 8..h + 12].try_into().unwrap()) as u64;
            let rva = u32::from_le_bytes(bytes[h + 12..h + 16].try_into().unwrap()) as u64;
            let raw = u32::from_le_bytes(bytes[h + 20..h + 24].try_into().unwrap()) as u64;
            if &name[..5] == b".text" {
                text_rva = Some(rva);
                text_vsize = Some(vsize);
            }
            if pdata_rva as u64 >= rva && (pdata_rva as u64) < rva + vsize.max(1) {
                pdata_file_off = Some(raw + (pdata_rva as u64 - rva));
            }
        }
        let pos = pdata_file_off.expect("Exception Directory RVA maps into some section");
        let text_rva = text_rva.expect(".text section present");
        let text_vsize = text_vsize.expect(".text section present");

        // RUNTIME_FUNCTION: BeginAddress RVA, then UnwindData RVA (since
        // Flag==0 in the low 2 bits means "points at .xdata", not packed).
        let begin_rva =
            u32::from_le_bytes(bytes[pos as usize..pos as usize + 4].try_into().unwrap()) as u64;
        let unwind_data_rva = u32::from_le_bytes(
            bytes[pos as usize + 4..pos as usize + 8].try_into().unwrap(),
        );
        assert_eq!(begin_rva, text_rva, "RUNTIME_FUNCTION must cover .text from its start");
        assert_eq!(unwind_data_rva & 0x3, 0, "Flag must be 0 (.xdata pointer, not packed)");

        // .xdata header word.
        let xdata_pos = pos + (unwind_data_rva as u64 - pdata_rva as u64);
        let header = u32::from_le_bytes(
            bytes[xdata_pos as usize..xdata_pos as usize + 4].try_into().unwrap(),
        );
        let function_length = header & 0x3FFFF; // bits [17:0]
        let vers = (header >> 18) & 0x3;
        let x_bit = (header >> 20) & 0x1;
        let e_bit = (header >> 21) & 0x1;
        let epilog_count = (header >> 22) & 0x1F;
        let code_words = (header >> 27) & 0x1F;
        assert_eq!(vers, 0, "only unwind-info version 0 is defined");
        assert_eq!(x_bit, 0, "no exception handler in this minimal record");
        assert_eq!(epilog_count, 0, "entry point never executes a normal return");
        assert_eq!(code_words, 1, "alloc_s + end fit in exactly one word");
        assert_eq!(
            function_length as u64 * 4,
            text_vsize,
            "Function Length (in 4-byte units) must cover the whole of .text, \
             since sp is never restored until the process exits"
        );
        let _ = e_bit; // documented as 0 above; no epilogue-scope array to walk

        // Unwind codes word: alloc_s (sub sp,sp,#64) then end.
        let codes_pos = xdata_pos + 4;
        let codes = &bytes[codes_pos as usize..codes_pos as usize + 4];
        assert_eq!(codes[0] & 0xE0, 0, "alloc_s tag bits must be 000");
        assert_eq!(
            (codes[0] & 0x1F) as u32 * 16,
            ARM64_ENTRY_FRAME_BYTES,
            "alloc_s must encode the entry stub's actual frame size"
        );
        assert_eq!(codes[1], UNWIND_CODE_END, "sequence must terminate with end (0xE4)");
    }
}

