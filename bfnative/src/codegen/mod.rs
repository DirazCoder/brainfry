//! Shared machinery for the two machine-code backends (x86-64, ARM64).
//!
//! The backends emit raw instruction bytes into an [`Emitter`], referencing
//! anything they can't yet know — branch targets, string addresses, GOT/IAT
//! slots — through *fixups*. The object-format writers (`crate::obj`) decide
//! where the code and data land in the final image, then run one patch pass
//! that resolves every fixup to a real virtual address. That split is what
//! lets one emitter serve three executable formats without caring which.
//!
//! Register budgets are fixed per architecture and shared by all OS runtimes:
//!
//! - x86-64: `r12` = cell pointer, `r13` = tape base, `r14` = tape end
//!   (one past the last valid cell), plus on Windows only `rbx`/`r15` hold
//!   the stdout/stdin handles. All of these are callee-saved in every ABI we
//!   target, so they survive calls into libSystem / kernel32, and kernel
//!   syscalls preserve everything except the caller-saved scratch set.
//! - ARM64: `x19` = cell pointer, `x20` = base, `x21` = end, plus on Windows
//!   only `x22`/`x23` hold the std handles. `x18` is never touched (Windows
//!   reserves it for the TEB).
//!
//! The tape starts at [`TAPE_INITIAL`] cells and grows at runtime by committing
//! more pages of a large virtual reservation — see the `grow` runtime routine
//! each backend emits, and `README.md` for the full strategy.

pub mod aarch64;
pub mod x86_64;

use bfformat::Op;

use crate::target::{Arch, Os, Target};

/// Tape capacity at program start, matching `bfrun`'s `INITIAL_TAPE_SIZE`.
/// Unlike the old assembly backend (which froze a fixed 16 MiB buffer), the
/// native tape keeps `bfrun`'s grow-on-demand behavior: moving right past the
/// current capacity commits more memory instead of faulting.
pub const TAPE_INITIAL: u64 = 30_000;

/// Total virtual address space reserved for the tape at startup: 1 GiB.
/// Runtime growth commits pages out of this reservation (mprotect on
/// Linux/macOS, VirtualAlloc(MEM_COMMIT) on Windows) and never moves the
/// tape, so the cell pointer register stays valid without rebasing. A
/// program that walks past this gets a clean runtime error (exit 1), not a
/// crash — this is the one documented divergence from `bfrun`, whose tape
/// is unbounded.
pub const TAPE_RESERVE: u64 = 1 << 30;

/// Granularity of every capacity/commit computation in the grow routine.
/// 64 KiB is a multiple of every page size an ARM64 or x86-64 kernel can use
/// (4 KiB, 16 KiB, 64 KiB), so every address we pass to mprotect /
/// VirtualAlloc is aligned without having to query the runtime page size
/// from auxv. The only cost is slightly coarser growth.
pub const TAPE_GRAN: u64 = 64 * 1024;

/// Runtime error messages. The underflow and I/O texts mirror `bfrun`'s
/// exact wording so scripts checking one implementation's stderr don't
/// diverge on the other. All of these end with a newline and go to stderr,
/// and every one exits the process with status 1.
pub const MSG_UNDERFLOW: &[u8] = b"runtime error: pointer moved left of cell 0\n";
pub const MSG_IO: &[u8] = b"runtime error: I/O error\n";
pub const MSG_CAP: &[u8] = b"runtime error: tape limit exceeded (1073741824 bytes)\n";
pub const MSG_GROW: &[u8] = b"runtime error: failed to grow tape\n";
pub const MSG_ALLOC: &[u8] = b"runtime error: failed to reserve tape memory\n";

/// GOT (macOS) / IAT (Windows) slot indices. Linux needs none of these —
/// I/O and memory go through raw syscalls. The same index works for both
/// tables because each OS uses exactly one of them.
pub const GOT_WRITE: u32 = 0;
pub const GOT_READ: u32 = 1;
pub const GOT_MMAP: u32 = 2;
pub const GOT_MPROTECT: u32 = 3;
pub const GOT_EXIT: u32 = 4;

/// The external functions each OS runtime binds, in GOT/IAT slot order.
/// Mach-O symbol names get a leading underscore added by the Mach-O writer;
/// PE import names are used verbatim.
pub fn external_symbols(os: Os) -> &'static [&'static str] {
    match os {
        Os::Linux => &[],
        Os::Macos => &["write", "read", "mmap", "mprotect", "_exit"],
        Os::Windows => &[
            "GetStdHandle",
            "WriteFile",
            "ReadFile",
            "ExitProcess",
            "VirtualAlloc",
        ],
    }
}

/// What a fixup points at. Resolved to a virtual address by the object
/// writer once the image layout is known.
#[derive(Debug, Clone, Copy)]
pub enum PatchTarget {
    /// Label `i`. Labels `0..ops.len()` are the ops themselves (label `i`
    /// is where the code for op `i` starts), `ops.len()` is the exit
    /// sequence, and anything beyond that is a runtime-internal label.
    Label(u32),
    /// Byte offset into the read-only data blob (the error messages).
    Str(u32),
    /// GOT/IAT slot index (see [`external_symbols`]).
    Got(u32),
}

/// How to patch the bytes at the fixup position once the target address is
/// known.
#[derive(Debug, Clone, Copy)]
pub enum PatchKind {
    /// 4-byte little-endian displacement at `pos`, relative to the end of
    /// the field: `disp = target - (vaddr(pos) + 4)`. Used for every x86-64
    /// rip-relative reference and every rel32 branch (all conditional
    /// branches are emitted in the long 6-byte `0F 8x` form so their size
    /// never depends on distance).
    X86Rel32,
    /// ARM64 `B`/`BL` (26-bit signed word offset).
    ArmB,
    /// ARM64 `CBZ`/`CBNZ`/`B.cond` (19-bit signed word offset). The
    /// backends only ever use these for jumps of a few bytes (to an adjacent
    /// trampoline), so range is never a concern, but the patcher still
    /// checks.
    ArmCond,
    /// ARM64 `ADRP` at `pos` with its paired instruction at `pos + 4`. The
    /// pair is `LDR Xt, [Xt, #imm]` for GOT/IAT slots (target must be
    /// 8-byte aligned) or `ADD Xt, Xt, #imm` for string addresses (any
    /// 12-bit offset works).
    ArmAdrp { pair: ArmAdrpPair },
}

#[derive(Debug, Clone, Copy)]
pub enum ArmAdrpPair {
    Ldr,
    Add,
}

#[derive(Debug, Clone, Copy)]
pub struct Fixup {
    pub pos: u32,
    pub kind: PatchKind,
    pub target: PatchTarget,
}

/// One annotation for the `--emit-asm` listing: "starting at code offset
/// `off`, the following bytes mean `note`". Backends push these while
/// emitting so the listing can show real mnemonics without a disassembler.
#[derive(Debug)]
pub struct Span {
    pub off: u32,
    pub note: String,
}

/// Byte-accumulator shared by both backends. Owns the code, the label table,
/// the fixup list, the read-only data blob, and listing spans.
pub struct Emitter {
    pub code: Vec<u8>,
    /// `labels[i]` = code offset of label `i`, or `u32::MAX` while unbound.
    pub labels: Vec<u32>,
    pub fixups: Vec<Fixup>,
    pub rodata: Vec<u8>,
    pub spans: Vec<Span>,
}

impl Emitter {
    /// Labels `0..=op_count` are pre-allocated: one per op plus the exit.
    /// Runtime routines allocate more through [`Emitter::internal_label`].
    pub fn new(op_count: usize) -> Self {
        Emitter {
            code: Vec::new(),
            labels: vec![u32::MAX; op_count + 1],
            fixups: Vec::new(),
            rodata: Vec::new(),
            spans: Vec::new(),
        }
    }

    pub fn cur(&self) -> u32 {
        self.code.len() as u32
    }

    pub fn internal_label(&mut self) -> u32 {
        self.labels.push(u32::MAX);
        (self.labels.len() - 1) as u32
    }

    pub fn bind_here(&mut self, label: u32) {
        self.labels[label as usize] = self.cur();
    }

    // ---- raw byte emitters ----

    pub fn u8(&mut self, byte: u8) {
        self.code.push(byte);
    }

    pub fn bytes(&mut self, bytes: &[u8]) {
        self.code.extend_from_slice(bytes);
    }

    pub fn u32(&mut self, v: u32) {
        self.code.extend_from_slice(&v.to_le_bytes());
    }

    // ---- fixup emitters (emit placeholder bytes + record) ----

    /// x86-64: emits 4 zero bytes to be filled with a rel32/rip32
    /// displacement.
    pub fn x86_rel32(&mut self, target: PatchTarget) {
        let pos = self.cur();
        self.u32(0);
        self.fixups.push(Fixup {
            pos,
            kind: PatchKind::X86Rel32,
            target,
        });
    }

    /// ARM64: emits a B/BL placeholder word (opcode bits only) for a
    /// far unconditional jump.
    pub fn arm_b(&mut self, is_bl: bool, target: PatchTarget) {
        let pos = self.cur();
        self.u32(if is_bl { 0x9400_0000 } else { 0x1400_0000 });
        self.fixups.push(Fixup {
            pos,
            kind: PatchKind::ArmB,
            target,
        });
    }

    /// ARM64: emits a CBZ/CBNZ/B.cond placeholder word. The condition /
    /// register fields must already be in the base word; the patcher fills
    /// the immediate field.
    pub fn arm_cond(&mut self, base_word: u32, target: PatchTarget) {
        debug_assert_eq!(base_word & 0x00FF_FFE0, 0, "immediate field must be zero");
        let pos = self.cur();
        self.u32(base_word);
        self.fixups.push(Fixup {
            pos,
            kind: PatchKind::ArmCond,
            target,
        });
    }

    /// ARM64: emits `ADRP Xn, <target>` + a paired LDR/ADD placeholder word.
    /// Both words get patched from the same fixup.
    pub fn arm_adrp_pair(&mut self, reg: u32, pair: ArmAdrpPair, target: PatchTarget) {
        let pos = self.cur();
        self.u32(0x9000_0000 | reg); // ADRP with zero immediate
        let pair_word = match pair {
            ArmAdrpPair::Ldr => 0xF940_0000 | (reg << 5) | reg, // LDR Xn, [Xn, #0]
            ArmAdrpPair::Add => 0x9100_0000 | (reg << 5) | reg, // ADD Xn, Xn, #0
        };
        self.u32(pair_word);
        self.fixups.push(Fixup {
            pos,
            kind: PatchKind::ArmAdrp { pair },
            target,
        });
    }

    // ---- data / listing ----

    /// Appends `bytes` to the read-only blob (NUL-terminated) and returns
    /// the blob offset.
    pub fn string(&mut self, bytes: &[u8]) -> u32 {
        let off = self.rodata.len() as u32;
        self.rodata.extend_from_slice(bytes);
        self.rodata.push(0);
        off
    }

    pub fn span(&mut self, note: impl Into<String>) {
        let off = self.cur();
        self.spans.push(Span {
            off,
            note: note.into(),
        });
    }
}

/// The finished output of one backend: everything the object writers need.
pub struct Module {
    pub code: Vec<u8>,
    pub labels: Vec<u32>,
    pub fixups: Vec<Fixup>,
    pub rodata: Vec<u8>,
    pub spans: Vec<Span>,
}

impl Module {
    /// Overwrites 4 little-endian bytes at `pos` (patch pass).
    pub fn write32(&mut self, pos: u32, v: u32) {
        self.code[pos as usize..pos as usize + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// Reads 4 little-endian bytes at `pos` (ARM64 patching is RMW).
    pub fn read32(&self, pos: u32) -> u32 {
        u32::from_le_bytes(
            self.code[pos as usize..pos as usize + 4]
                .try_into()
                .expect("fixup position in range"),
        )
    }
}

/// Compiles optimized ops to raw machine code for `target`.
pub fn emit(ops: &[Op], target: Target) -> Module {
    let mut emitter = Emitter::new(ops.len());
    match target.arch {
        Arch::X86_64 => x86_64::emit(&mut emitter, ops, target),
        Arch::Aarch64 => aarch64::emit(&mut emitter, ops, target),
    }

    for (i, off) in emitter.labels.iter().enumerate() {
        assert_ne!(*off, u32::MAX, "label {i} was never bound");
    }
    assert!(
        emitter.fixups.iter().all(|f| match f.target {
            PatchTarget::Got(i) => (i as usize) < external_symbols(target.os).len(),
            _ => true,
        }),
        "GOT fixup out of range"
    );

    Module {
        code: emitter.code,
        labels: emitter.labels,
        fixups: emitter.fixups,
        rodata: emitter.rodata,
        spans: emitter.spans,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_labels_allocate_sequentially() {
        let mut e = Emitter::new(3);
        assert_eq!(e.internal_label(), 4);
        assert_eq!(e.internal_label(), 5);
        e.bind_here(4);
        assert_eq!(e.labels[4], 0);
    }

    #[test]
    fn strings_are_nul_terminated_and_offset_correctly() {
        let mut e = Emitter::new(0);
        let a = e.string(b"hi");
        let b = e.string(b"yo");
        assert_eq!((a, b), (0, 3));
        assert_eq!(&e.rodata, b"hi\0yo\0");
    }

    #[test]
    fn fixups_record_positions_of_placeholder_bytes() {
        let mut e = Emitter::new(0);
        e.x86_rel32(PatchTarget::Label(0));
        assert_eq!(e.code, vec![0, 0, 0, 0]);
        assert_eq!(e.fixups.len(), 1);
        assert!(matches!(e.fixups[0].kind, PatchKind::X86Rel32));
    }
}
