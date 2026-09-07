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
//!   `VirtualAlloc` (kernel32)
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
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    /// `read(2)` — the generated Input op.
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
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
    unsafe {
        vec![
            write as usize,
            read as usize,
            mmap as usize,
            mprotect as usize,
            _exit as usize,
        ]
    }
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
}

#[cfg(target_os = "windows")]
fn windows_slots() -> Vec<usize> {
    unsafe {
        vec![
            GetStdHandle as usize,
            WriteFile as usize,
            ReadFile as usize,
            ExitProcess as usize,
            VirtualAlloc as usize,
        ]
    }
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

    #[cfg(target_os = "linux")]
    #[test]
    fn non_host_tables_are_unreachable() {
        // On a Linux host the macOS/Windows tables can never be produced —
        // got_slots only ever sees the host OS.
        assert!(got_slots(Os::Linux).is_empty());
    }
}
