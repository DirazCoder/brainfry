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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::{self, Span};
    use crate::target::Target;
    use bfformat::Op;

    fn layout() -> Layout {
        Layout {
            code_vaddr: 0x4000,
            rodata_vaddr: 0x8000,
            got_vaddr: 0,
        }
    }

    #[test]
    fn render_describes_every_container_format() {
        // The header's format line is target-specific; all three OSes must
        // be named (the e2e suite only ever lists for the host target).
        for (name, expected) in [
            ("linux-x86_64", "ELF64 static executable"),
            ("macos-x86_64", "Mach-O 64-bit PIE"),
            ("windows-x86_64", "PE32+ executable"),
        ] {
            let target = Target::from_name(name).unwrap();
            let module = codegen::emit(&[Op::Zero], target);
            let text = render(&module, target, &layout());
            assert!(text.contains(expected), "{name}: {text}");
            assert!(text.contains(&format!("; target:  {target}")), "{text}");
        }
    }

    #[test]
    fn zero_length_spans_get_a_header_line_or_are_skipped_by_convention() {
        // A span with no bytes and a note that doesn't start with a space is
        // informational (structure-only): it gets a header line so the
        // listing still shows it. A space-prefixed note (continuation of the
        // previous span, like "  grow: call bf_grow") renders no line at
        // all -- there are no bytes to show and the note isn't a headline.
        let module = Module {
            code: vec![0x90],
            labels: vec![0],
            fixups: Vec::new(),
            rodata: Vec::new(),
            spans: vec![
                Span {
                    off: 0,
                    note: "real span".to_string(),
                },
                Span {
                    off: 1,
                    note: "empty informational span".to_string(),
                },
                Span {
                    off: 1,
                    note: "  skipped continuation".to_string(),
                },
            ],
        };
        let text = render(&module, Target::from_name("linux-x86_64").unwrap(), &layout());
        assert!(
            text.contains("empty informational span"),
            "informational span missing: {text}"
        );
        assert!(
            !text.contains("skipped continuation"),
            "space-prefixed zero span should render nothing: {text}"
        );
        assert!(text.contains("real span"), "{text}");
    }

    #[test]
    fn rodata_is_omitted_when_empty_and_rendered_when_present() {
        let target = Target::from_name("linux-x86_64").unwrap();
        // An empty rodata means no error messages -- a synthetic module.
        let empty = Module {
            code: vec![0x90],
            labels: vec![0],
            fixups: Vec::new(),
            rodata: Vec::new(),
            spans: Vec::new(),
        };
        assert!(!render(&empty, target, &layout()).contains("\n; read-only data at"));

        // A real emission always registers the runtime error strings, so
        // its listing has the rodata section (with printable rendering of
        // the bytes).
        let module = codegen::emit(&[Op::Zero], target);
        assert!(render(&module, target, &layout()).contains("\n; read-only data at"));
    }
}
