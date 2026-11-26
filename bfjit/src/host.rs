//! Host-side resolution of the functions the generated code calls.
//!
//! `bfnative` binaries bind these through their container: dyld fills the
//! Mach-O GOT from libSystem, the PE loader fills the IAT from kernel32.
//! The JIT has no container and no loader, so this module is the "dynamic
//! link" step: it takes the addresses of the same functions out of the
//! process `bfjit` itself already links against and writes them into the
//! GOT/IAT slots inside the JIT region before the W^X flip.
//!
//! Slot order is [`crate::codegen::external_symbols`] — the exact order the
//! codegen emits GOT/IAT references in:
//!
//! - macOS: `write`, `read`, `mmap`, `mprotect`, `_exit` (libSystem)
//! - Windows: `GetStdHandle`, `WriteFile`, `ReadFile`, `ExitProcess`,
//!   `VirtualAlloc`, `GetLastError` (kernel32)
//! - Linux: nothing — the generated code talks to the kernel through raw
//!   `syscall` instructions and has no external calls at all.
//!
//! Signatures below are shaped to match how the codegen sets up arguments
//! (the `RunError`/I/O paths in the backends document each call site).

use crate::target::Os;

/// GOT/IAT slot contents as raw addresses, in
/// [`crate::codegen::external_symbols`] order. Empty on Linux.
pub fn got_slots(os: Os) -> Vec<usize> {
    match os {
        Os::Linux => Vec::new(),
        Os::Macos => macos_slots(),
        Os::Windows => windows_slots(),
    }
}

// ---------------------------------------------------------------------------
// macOS: libSystem
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `write(2)` — the generated Output op and the error tail.
    fn write(fd: i32, buf: *const core::ffi::c_void, count: usize) -> isize;
    /// `read(2)` — the generated Input op.
    fn read(fd: i32, buf: *mut core::ffi::c_void, count: usize) -> isize;
    /// `mmap(2)` — tape reservation in the entry, and nothing else.
    fn mmap(
        addr: *mut core::ffi::c_void,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut core::ffi::c_void;
    /// `mprotect(2)` — initial tape commit and growth commits.
    fn mprotect(addr: *mut core::ffi::c_void, len: usize, prot: i32) -> i32;
    /// `_exit(2)` — the process-terminating success/error exits. The
    /// generated code never returns, so it never observes the `!` type;
    /// the pointer only needs the right address.
    fn _exit(status: i32);
}

#[cfg(target_os = "macos")]
fn macos_slots() -> Vec<usize> {
    vec![
        write as *const () as usize,
        read as *const () as usize,
        mmap as *const () as usize,
        mprotect as *const () as usize,
        _exit as *const () as usize,
    ]
}

#[cfg(not(target_os = "macos"))]
fn macos_slots() -> Vec<usize> {
    // Only reachable if Target::host() ever disagreed with cfg(target_os)
    // about the running OS, which cannot happen (host() is derived from the
    // same cfgs).
    unreachable!("macOS GOT slots requested on a non-macOS host")
}

// ---------------------------------------------------------------------------
// Windows: kernel32
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
unsafe extern "C" {
    fn GetStdHandle(n_std_handle: u32) -> *mut core::ffi::c_void;
    #[allow(clippy::too_many_arguments)]
    fn WriteFile(
        handle: *mut core::ffi::c_void,
        buffer: *const u8,
        bytes_to_write: u32,
        bytes_written: *mut u32,
        overlapped: *const core::ffi::c_void,
    ) -> i32;
    fn ReadFile(
        handle: *mut core::ffi::c_void,
        buffer: *mut u8,
        bytes_to_read: u32,
        bytes_read: *mut u32,
        overlapped: *const core::ffi::c_void,
    ) -> i32;
    fn ExitProcess(exit_code: u32);
    fn VirtualAlloc(
        address: *mut core::ffi::c_void,
        size: usize,
        allocation_type: u32,
        protect: u32,
    ) -> *mut core::ffi::c_void;
    fn GetLastError() -> u32;
}

#[cfg(target_os = "windows")]
fn windows_slots() -> Vec<usize> {
    vec![
        GetStdHandle as *const () as usize,
        WriteFile as *const () as usize,
        ReadFile as *const () as usize,
        ExitProcess as *const () as usize,
        VirtualAlloc as *const () as usize,
        GetLastError as *const () as usize,
    ]
}

#[cfg(not(target_os = "windows"))]
fn windows_slots() -> Vec<usize> {
    unreachable!("Windows IAT slots requested on a non-Windows host")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_needs_no_slots() {
        assert!(got_slots(Os::Linux).is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_slots_match_codegen_table() {
        let slots = got_slots(Os::Macos);
        assert_eq!(
            slots.len(),
            crate::codegen::external_symbols(Os::Macos).len()
        );
        assert!(slots.iter().all(|slot| *slot != 0));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_slots_match_codegen_table() {
        let slots = got_slots(Os::Windows);
        assert_eq!(
            slots.len(),
            crate::codegen::external_symbols(Os::Windows).len()
        );
        assert!(slots.iter().all(|slot| *slot != 0));
    }

    // The macOS/Windows tables can only be requested by a host/OS mismatch
    // that cannot happen (Target::host() derives from the same cfg!s the
    // table selection compiles on). Their unreachable!() guards are the
    // statement of that invariant, and these tests pin both the guard and
    // the message by actually driving the non-host arms on a Linux host.
    // (On a real macOS/Windows host the table exists, so the tests are
    // compiled out there.)
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    #[test]
    #[should_panic(expected = "macOS GOT slots requested on a non-macOS host")]
    fn macos_slots_are_unreachable_on_this_host() {
        let _ = got_slots(Os::Macos);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    #[should_panic(expected = "Windows IAT slots requested on a non-Windows host")]
    fn windows_slots_are_unreachable_on_this_host() {
        let _ = got_slots(Os::Windows);
    }
}
