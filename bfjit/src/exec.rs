//! Executable-memory management — the one place `bfjit` talks to the OS
//! about memory protection, and the layer that replaces `bfnative`'s
//! ELF/Mach-O/PE writers: instead of serializing machine code into a file
//! the loader maps in, the code is placed in a private anonymous mapping
//! and executed in place.
//!
//! W^X discipline is the whole point of this module: a page is never
//! writable and executable at the same time, on any platform.
//!
//! - **Linux**: `mmap(PROT_READ|PROT_WRITE)` → copy code →
//!   `mprotect(PROT_READ|PROT_EXEC)`. On aarch64, the canonical cache
//!   maintenance sequence (DC CVAU / IC IVAU / DSB / ISB) before the first
//!   execution — the ARM instruction cache is not coherent with the data
//!   cache, and freshly-written code is invisible to it until the lines
//!   are cleaned and invalidated (on x86-64 this is a hardware no-op).
//! - **macOS (x86-64 and ARM64)**: same plain `mmap`/`mprotect` pair as
//!   Linux. `MAP_JIT` is deliberately not used on either architecture:
//!   `mmap(MAP_JIT)` requires the `com.apple.security.cs.allow-jit`
//!   entitlement, and that entitlement is only honored by the kernel on a
//!   Hardened Runtime-signed binary carrying a real Developer ID signature
//!   — an ad-hoc signature can embed the entitlement in its blob, but the
//!   kernel still refuses the mapping (`mmap` fails with `ENOMEM`). This
//!   build targets plain ad-hoc signing with no Hardened Runtime, so
//!   `MAP_JIT` would simply fail here regardless of architecture. The
//!   plain `mmap`+`mprotect` W^X flip works without any entitlement as
//!   long as the binary isn't Hardened Runtime-signed, on both Intel and
//!   Apple Silicon. `sys_icache_invalidate` still runs after the flip to
//!   executable (mandatory on ARM64; a no-op on x86-64), since the
//!   non-coherent instruction cache is a CPU property, independent of
//!   which memory-protection mechanism produced the code.
//! - **Windows**: `VirtualAlloc(PAGE_READWRITE)` → copy code →
//!   `VirtualProtect(PAGE_EXECUTE_READ)`, then `FlushInstructionCache`
//!   (required on ARM64, harmless on x86-64).
//!
//! The region also hosts the read-only data blob and the GOT/IAT table the
//! generated code references — all written during the writable phase, all
//! read-only once execution begins. `bfnative`'s Mach-O/PE writers do the
//! same thing through dyld/loader relocations; here the table is filled
//! with host function pointers before the W^X flip.

use core::ffi::c_void;

/// An anonymous private region that is writable at first and becomes
/// read+execute via [`JitRegion::make_executable`].
///
/// No `Drop`: the generated code entered through [`JitRegion::enter`] never
/// returns (it terminates the process itself, exactly like a `bfnative`
/// executable would), and on every error path the process is about to exit
/// anyway — unmapping is not a meaningful cleanup in either case.
pub struct JitRegion {
    pub base: *mut u8,
    pub len: usize,
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!("bfjit only supports Linux, macOS, and Windows hosts");

impl JitRegion {
    /// Maps `len` bytes of private anonymous memory, writable and readable
    /// but *not* executable — execution permission only appears in
    /// [`JitRegion::make_executable`], after the code is in place.
    ///
    /// On macOS (both x86-64 and ARM64) this is a plain anonymous mapping,
    /// no `MAP_JIT` — see the module doc comment for why.
    ///
    /// # Safety
    ///
    /// `len` must be nonzero and a reasonable mapping size.
    pub unsafe fn alloc(len: usize) -> Result<JitRegion, String> {
        unsafe { alloc_region(len) }
    }

    /// Copies `bytes` to `base + off`. Only valid before
    /// `make_executable` (the region is writable exactly then).
    ///
    /// # Safety
    ///
    /// `off + bytes.len()` must be within the region.
    pub unsafe fn write_bytes(&self, off: usize, bytes: &[u8]) {
        debug_assert!(off + bytes.len() <= self.len);
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.add(off), bytes.len());
        }
    }

    /// Writes one 8-byte value at `base + off` (GOT/IAT slot fill).
    ///
    /// # Safety
    ///
    /// `off + 8` must be within the region.
    pub unsafe fn write_u64(&self, off: usize, value: u64) {
        debug_assert!(off + 8 <= self.len);
        unsafe {
            core::ptr::write_unaligned(self.base.add(off) as *mut u64, value);
        }
    }

    /// Flips the region from writable to read+execute — the W^X transition
    /// — and makes the freshly written code visible to the instruction
    /// cache where the architecture requires it.
    pub unsafe fn make_executable(&self) -> Result<(), String> {
        unsafe { protect_exec(self.base, self.len) }
    }

    /// Jumps into the generated code at `base` — the entry stub the
    /// backends emit as the first thing in the code buffer, the same
    /// address an ELF `e_entry` / Mach-O `LC_MAIN` / PE entry point would
    /// point at.
    ///
    /// The entry never returns: it runs the program and terminates the
    /// process with the program's exit status, so this call has type `!`.
    // coverage(off) in coverage runs: these lines execute *after* the
    // final coverage counter dump by construction — main() flushes the
    // profile immediately before jumping here because the generated entry
    // exits via raw syscall, skipping the atexit-based writer. No profile
    // can ever contain these lines; the behavior itself is pinned by the
    // e2e suite (every successful run goes through here).
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub unsafe fn enter(&self) -> ! {
        // macOS-ARM64 special case: the generated entry assumes dyld's
        // LC_MAIN stack alignment (sp 16-byte aligned at entry) and calls
        // libSystem without ever realigning sp itself — on every other
        // target the entry either realigns the stack (macos/windows-x86_64,
        // windows-aarch64) or never makes a call that could care
        // (linux uses raw syscalls on both architectures). Hand the macOS
        // ARM64 entry the alignment it expects by truncating sp down to a
        // 16-byte boundary before branching; the trashed return address is
        // irrelevant because the code never returns.
        //
        // AND cannot target sp directly on ARM64, so the alignment goes
        // through a scratch register: mov x16, sp; and x16, x16, #-16;
        // mov sp, x16 (ADD-sp alias); br entry.
        //
        // The entry address is pinned to x17 (IP2, the second architectural
        // call scratch) rather than left to `in(reg)` allocation: the
        // template clobbers x16 by name, and LLVM does not parse template
        // text, so a generic `in(reg)` operand *could* be assigned x16 by
        // some register-allocator decision — after which `mov x16, sp`
        // would destroy the entry address and `br` would jump to a garbage
        // (stack) address. x17 is written by nothing in this template, so
        // `in("x17")` makes the trampoline correct by construction instead
        // of by allocator luck. (An earlier revision declared the x16
        // clobber with `out("x16") _`; that declaration was correct and its
        // removal is what made this fragile.)
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        unsafe {
            let entry = self.base as usize;
            core::arch::asm!(
                "mov x16, sp",
                "and x16, x16, #-16",
                "mov sp, x16",
                "br x17",
                in("x17") entry,
                options(noreturn),
            )
        }

        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        unsafe {
            let entry: unsafe extern "C" fn() -> ! = core::mem::transmute(self.base);
            entry()
        }
    }
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut c_void;
    fn mprotect(addr: *mut c_void, len: usize, prot: i32) -> i32;
}

#[cfg(target_os = "linux")]
mod linux {
    pub const PROT_READ: i32 = 1;
    pub const PROT_WRITE: i32 = 2;
    pub const PROT_EXEC: i32 = 4;
    pub const MAP_PRIVATE: i32 = 0x02;
    pub const MAP_ANONYMOUS: i32 = 0x20;
}

#[cfg(target_os = "linux")]
unsafe fn alloc_region(len: usize) -> Result<JitRegion, String> {
    unsafe {
        let prot = linux::PROT_READ | linux::PROT_WRITE;
        let flags = linux::MAP_PRIVATE | linux::MAP_ANONYMOUS;
        let base = mmap(core::ptr::null_mut(), len, prot, flags, -1, 0);
        if base as isize == -1 {
            // MAP_FAILED
            return Err(format!(
                "failed to map JIT memory ({} bytes): {}",
                len,
                std::io::Error::last_os_error()
            ));
        }
        Ok(JitRegion {
            base: base as *mut u8,
            len,
        })
    }
}

#[cfg(target_os = "linux")]
unsafe fn protect_exec(base: *mut u8, len: usize) -> Result<(), String> {
    unsafe {
        let prot = linux::PROT_READ | linux::PROT_EXEC;
        if mprotect(base as *mut c_void, len, prot) != 0 {
            return Err(format!(
                "failed to make JIT memory executable: {}",
                std::io::Error::last_os_error()
            ));
        }
        // The data cache and instruction cache are incoherent on ARM64: code
        // written through normal stores is not guaranteed visible to
        // instruction fetch until the D-lines are cleaned and I-lines
        // invalidated (see clear_instruction_cache for the exact sequence).
        // On x86-64 the caches are coherent and this is unnecessary.
        #[cfg(target_arch = "aarch64")]
        clear_instruction_cache(base, len);

        Ok(())
    }
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
unsafe fn clear_instruction_cache(base: *mut u8, len: usize) {
    // The canonical self-modifying-code visibility sequence from the ARM
    // Architecture Reference Manual (same one compiler-rt's __clear_cache
    // implements): clean the D-cache lines of the range to the Point of
    // Unification, make those completes visible (DSB), invalidate the
    // I-cache lines to the Point of Unification, DSB again, and ISB on
    // this thread. DC CVAU / IC IVAU are both permitted at EL0, and the
    // line sizes come from CTR_EL0 (readable at EL0), so the sequence is
    // fully self-contained — no libc `__clear_cache` dependency, which
    // some C libraries don't provide at all.
    unsafe {
        let start = base as usize;
        let end = start + len; // exclusive
        if len == 0 {
            return;
        }
        core::arch::asm!(
            "mrs  x9, ctr_el0",          // cache type register
            "ubfx x10, x9, #0, #4",      // DminLine: log2(D-line words)
            "mov  w11, #4",
            "lsl  w11, w11, w10",        // D line size in bytes
            "ubfx x10, x9, #16, #4",     // IminLine: log2(I-line words)
            "mov  w12, #4",
            "lsl  w12, w12, w10",        // I line size in bytes
            // Clean D-cache to PoU over [start, end).
            "mov  x8, x0",
            "1: dc cvau, x8",
            "add  x8, x8, x11",
            "cmp  x8, x1",
            "b.lo 1b",
            "dsb  ish",
            // Invalidate I-cache to PoU over [start, end).
            "mov  x8, x0",
            "2: ic ivau, x8",
            "add  x8, x8, x12",
            "cmp  x8, x1",
            "b.lo 2b",
            "dsb  ish",
            "isb",
            in("x0") start,
            in("x1") end,
            out("x8") _,
            out("x9") _,
            out("x10") _,
            out("x11") _,
            out("x12") _,
        );
    }
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut c_void;
    fn mprotect(addr: *mut c_void, len: usize, prot: i32) -> i32;
    /// Invalidate instruction cache lines for a range — required on ARM64
    /// before executing freshly stored code.
    fn sys_icache_invalidate(start: *mut c_void, len: usize);
}

#[cfg(target_os = "macos")]
mod macos {
    pub const PROT_READ: i32 = 1;
    pub const PROT_WRITE: i32 = 2;
    pub const PROT_EXEC: i32 = 4;
    pub const MAP_PRIVATE: i32 = 0x02;
    pub const MAP_ANONYMOUS: i32 = 0x1000;
    // MAP_JIT (0x8000) deliberately omitted: see the module doc comment
    // and alloc_region for why this build never requests it.
}

#[cfg(target_os = "macos")]
unsafe fn alloc_region(len: usize) -> Result<JitRegion, String> {
    unsafe {
        let prot = macos::PROT_READ | macos::PROT_WRITE;
        // MAP_JIT requires the com.apple.security.cs.allow-jit entitlement
        // to be accepted at all, and that entitlement is only meaningful
        // (and only trusted by the kernel) on a Hardened Runtime-signed
        // binary with a real Developer ID — an ad-hoc signature can carry
        // the entitlement in its blob, but the kernel won't honor it, and
        // mmap(MAP_JIT) fails with ENOMEM regardless of architecture. This
        // build targets plain ad-hoc signing with no Hardened Runtime, so
        // every macOS architecture — arm64 included — uses the same plain
        // mmap+mprotect W^X sequence Intel always used: never request
        // PROT_WRITE and PROT_EXEC at once, flip between them instead.
        let flags = macos::MAP_PRIVATE | macos::MAP_ANONYMOUS;

        let base = mmap(core::ptr::null_mut(), len, prot, flags, -1, 0);
        if base as isize == -1 {
            return Err(format!(
                "failed to map JIT memory ({} bytes): {}",
                len,
                std::io::Error::last_os_error()
            ));
        }

        Ok(JitRegion {
            base: base as *mut u8,
            len,
        })
    }
}

#[cfg(target_os = "macos")]
unsafe fn protect_exec(base: *mut u8, len: usize) -> Result<(), String> {
    unsafe {
        // Plain mprotect on every macOS architecture, ad-hoc/no-Hardened-
        // Runtime build: see alloc_region for why MAP_JIT isn't used here.
        let prot = macos::PROT_READ | macos::PROT_EXEC;
        if mprotect(base as *mut c_void, len, prot) != 0 {
            return Err(format!(
                "failed to make JIT memory executable: {}",
                std::io::Error::last_os_error()
            ));
        }
        // ARM64's instruction cache isn't coherent with the data cache
        // regardless of which memory-protection mechanism produced the
        // code, so this stays unconditional; it's a no-op on x86-64.
        sys_icache_invalidate(base as *mut c_void, len);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
unsafe extern "C" {
    fn VirtualAlloc(
        address: *mut c_void,
        size: usize,
        allocation_type: u32,
        protect: u32,
    ) -> *mut c_void;
    fn VirtualProtect(
        address: *const c_void,
        size: usize,
        new_protect: u32,
        old_protect: *mut u32,
    ) -> i32;
    fn FlushInstructionCache(process: *mut c_void, base: *const c_void, size: usize) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
}

#[cfg(target_os = "windows")]
mod windows {
    pub const MEM_COMMIT: u32 = 0x1000;
    pub const MEM_RESERVE: u32 = 0x2000;
    pub const PAGE_READWRITE: u32 = 0x04;
    pub const PAGE_EXECUTE_READ: u32 = 0x20;
}

#[cfg(target_os = "windows")]
unsafe fn alloc_region(len: usize) -> Result<JitRegion, String> {
    unsafe {
        let ty = windows::MEM_COMMIT | windows::MEM_RESERVE;
        let base = VirtualAlloc(core::ptr::null_mut(), len, ty, windows::PAGE_READWRITE);
        if base.is_null() {
            return Err(format!(
                "failed to map JIT memory ({} bytes): {}",
                len,
                std::io::Error::last_os_error()
            ));
        }
        Ok(JitRegion {
            base: base as *mut u8,
            len,
        })
    }
}

#[cfg(target_os = "windows")]
unsafe fn protect_exec(base: *mut u8, len: usize) -> Result<(), String> {
    unsafe {
        let mut old = 0u32;
        if VirtualProtect(
            base as *const c_void,
            len,
            windows::PAGE_EXECUTE_READ,
            &mut old,
        ) == 0
        {
            return Err(format!(
                "VirtualProtect(PAGE_EXECUTE_READ) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // Harmless on x86-64, required on ARM64 (incoherent I-cache), and
        // cheap enough to always call.
        FlushInstructionCache(GetCurrentProcess(), base as *const c_void, len);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn clearing_an_empty_instruction_cache_range_is_a_no_op() {
        unsafe { clear_instruction_cache(core::ptr::null_mut(), 0) };
    }

    #[test]
    fn region_round_trip_writes_and_reads_back() {
        // Allocating a region, writing, and reading back is the writable
        // phase's whole contract; execution itself is covered by the e2e
        // suite (it can't be meaningfully unit-tested without actually
        // running generated code, which would make this test an e2e test).
        unsafe {
            let region = JitRegion::alloc(0x1000).expect("alloc");
            region.write_bytes(0, b"hello, jit");
            region.write_u64(0x100, 0xDEAD_BEEF_CAFE_F00D);

            assert_eq!(*region.base, b'h');
            assert_eq!(*region.base.add(9), b't');
            let val = core::ptr::read_unaligned(region.base.add(0x100) as *const u64);
            assert_eq!(val, 0xDEAD_BEEF_CAFE_F00D);

            // Leave it writable: the test never executes it, and the real
            // binary never frees either (see the type docs).
        }
    }

    #[test]
    fn an_impossible_allocation_maps_to_an_error() {
        // usize::MAX can never be mapped; the result must be a clean Err
        // (the CLI prints it and exits), never an abort.
        unsafe {
            let result = JitRegion::alloc(usize::MAX);
            let message = result.err().expect("usize::MAX must fail");
            assert!(message.contains("failed to map JIT memory"), "{message}");
        }
    }

    #[test]
    fn w_x_flip_succeeds_on_a_well_formed_region() {
        unsafe {
            let region = JitRegion::alloc(0x1000).expect("alloc");
            region.write_bytes(0, b"int3");
            region
                .make_executable()
                .expect("a well-formed region must become executable");
        }
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn executable_protection_failure_is_reported() {
        let region = JitRegion {
            base: core::ptr::null_mut(),
            len: 0x1000,
        };
        let message = unsafe { region.make_executable() }.expect_err("null region must fail");

        #[cfg(target_os = "macos")]
        assert!(
            message.contains("failed to make JIT memory executable"),
            "{message}"
        );

        #[cfg(target_os = "windows")]
        assert!(
            message.contains("VirtualProtect(PAGE_EXECUTE_READ) failed"),
            "{message}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mprotect_failure_is_reported_not_ignored() {
        // mprotect rejects a non-page-aligned address with EINVAL, which is
        // the one deterministic way to make the W^X flip fail; the error
        // must carry the OS message, and the region must be unaffected.
        unsafe {
            let region = JitRegion::alloc(0x1000).expect("alloc");
            let message =
                protect_exec(region.base.add(1), 0x1000).expect_err("unaligned mprotect must fail");
            assert!(
                message.contains("failed to make JIT memory executable"),
                "{message}"
            );
            // The failed flip leaves the region writable: prove the bytes
            // are still there and mutable.
            region.write_bytes(0, b"still writable");
            assert_eq!(*region.base, b's');
        }
    }
}
