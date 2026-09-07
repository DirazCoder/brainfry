//! `--emit-asm` output: an annotated hex/mnemonic listing of the generated
//! machine code.
//!
//! The old backend printed assembly *text* and handed it to an external
//! toolchain; this backend emits machine code directly, so assembly text no
//! longer exists. Rather than leave the flag broken (or drop it silently),
//! it now writes a listing of what was actually generated: code offset,
//! hex bytes, and a human annotation per span, with branch targets resolved
//! to final virtual addresses by the same layout pass that produces the
//! executable. It's a disassembly-shaped window into the real output, not a
//! synthetic intermediate.

use crate::codegen::Module;
use crate::obj::Layout;
use crate::target::Target;

pub fn render(module: &Module, target: Target, layout: &Layout) -> String {
    let format = match target.os {
        crate::target::Os::Linux => "ELF64 static executable (ET_EXEC, raw syscalls)",
        crate::target::Os::Macos => "Mach-O 64-bit PIE (MH_EXECUTE, libSystem via GOT)",
        crate::target::Os::Windows => "PE32+ executable (kernel32 via IAT)",
    };

    let mut out = String::new();
    out.push_str("; bfnative generated-code listing\n");
    out.push_str(&format!("; target:  {}\n", target));
    out.push_str(&format!("; format:  {format}\n"));
    out.push_str(&format!("; entry:   {:#x}\n", layout.code_vaddr));
    out.push_str(&format!(
        "; code:    {} bytes, read-only data: {} bytes, fixups: {}\n",
        module.code.len(),
        module.rodata.len(),
        module.fixups.len()
    ));
    out.push_str(&format!(
        "; tape:    {} cells initial, {} bytes reserved, grows at runtime\n",
        crate::codegen::TAPE_INITIAL,
        crate::codegen::TAPE_RESERVE
    ));
    out.push('\n');

    let spans = &module.spans;
    for (i, span) in spans.iter().enumerate() {
        let start = span.off as usize;
        let end = spans
            .get(i + 1)
            .map(|next| next.off as usize)
            .unwrap_or(module.code.len());
        if start == end && !span.note.starts_with(' ') {
            // Purely informational spans with no bytes still get a header
            // line so the listing shows the structure.
            out.push_str(&format!(
                "{:08x}                                  {}\n",
                layout.code_vaddr + start as u64,
                span.note
            ));
            continue;
        }
        let bytes = &module.code[start.min(module.code.len())..end.min(module.code.len())];
        let mut first = true;
        for (row, chunk) in bytes.chunks(16).enumerate() {
            let hexes: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
            let note = if first { span.note.as_str() } else { "" };
            out.push_str(&format!(
                "{:08x}  {:<47}  {}\n",
                layout.code_vaddr + (start + row * 16) as u64,
                hexes.join(" "),
                note
            ));
            first = false;
        }
    }

    if !module.rodata.is_empty() {
        out.push_str(&format!(
            "\n; read-only data at {:#x}:\n",
            layout.rodata_vaddr
        ));
        let mut offset = 0u64;
        for line in module.rodata.split(|b| *b == 0) {
            if line.is_empty() {
                continue;
            }
            let shown: String = line
                .iter()
                .map(|&b| {
                    if (0x20..0x7f).contains(&b) {
                        b as char
                    } else {
                        '.'
                    }
                })
                .collect();
            out.push_str(&format!(
                "{:08x}  {:<47}  \"{}\"\n",
                layout.rodata_vaddr + offset,
                format!("{:02x}", line.len()),
                shown
            ));
            offset += line.len() as u64 + 1;
        }
    }

    out
}
