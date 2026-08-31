use std::fmt;

/// Which instruction set code is generated for. Everything about how a
/// Brainfuck op becomes instructions is decided by this; how the program
/// starts, stops, and does I/O is decided by `Os`, so the six targets are
/// really two instruction backends with three thin runtime layers each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    /// Name as it appears inside target strings (`x86_64`, `aarch64`) and in
    /// `cc -arch` invocations.
    pub fn name(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        }
    }
}

/// Which operating system the executable targets. OS choice never changes
/// instruction selection — only the entry point, the I/O sequences, symbol
/// naming, and a couple of object-format quirks, all in the `os` module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Macos,
    Windows,
}

impl Os {
    pub fn name(self) -> &'static str {
        match self {
            Os::Linux => "linux",
            Os::Macos => "macos",
            Os::Windows => "windows",
        }
    }
}

/// A full compilation target: architecture plus operating system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub arch: Arch,
    pub os: Os,
}

/// The six supported targets, in the order they're listed in usage text.
/// 32-bit x86 and 32-bit ARM are deliberately absent — out of scope.
pub const ALL_TARGETS: [Target; 6] = [
    Target {
        arch: Arch::X86_64,
        os: Os::Linux,
    },
    Target {
        arch: Arch::Aarch64,
        os: Os::Linux,
    },
    Target {
        arch: Arch::X86_64,
        os: Os::Macos,
    },
    Target {
        arch: Arch::Aarch64,
        os: Os::Macos,
    },
    Target {
        arch: Arch::X86_64,
        os: Os::Windows,
    },
    Target {
        arch: Arch::Aarch64,
        os: Os::Windows,
    },
];

impl Target {
    /// Canonical name, like `linux-aarch64`.
    pub fn name(self) -> String {
        format!("{}-{}", self.os.name(), self.arch.name())
    }

    /// Parses one of the six canonical names. The error message lists every
    /// valid name so a typo explains itself.
    pub fn from_name(name: &str) -> Result<Target, String> {
        for candidate in ALL_TARGETS {
            if candidate.name() == name {
                return Ok(candidate);
            }
        }

        let valid = ALL_TARGETS
            .iter()
            .map(|target| target.name())
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!(
            "unknown target `{name}`; valid targets are: {valid}"
        ))
    }

    /// The target this bfnative binary itself is running on, i.e. what a bare
    /// `bfnative program.bf` compiles for.
    pub fn host() -> Target {
        let os = if cfg!(target_os = "linux") {
            Os::Linux
        } else if cfg!(target_os = "macos") {
            Os::Macos
        } else if cfg!(target_os = "windows") {
            Os::Windows
        } else {
            panic!("bfnative doesn't know how to pick a default target on this host OS")
        };

        let arch = if cfg!(target_arch = "x86_64") {
            Arch::X86_64
        } else if cfg!(target_arch = "aarch64") {
            Arch::Aarch64
        } else {
            panic!("bfnative doesn't know how to pick a default target on this host CPU")
        };

        Target { arch, os }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.os.name(), self.arch.name())
    }
}

/// Default assembler/linker driver command for a target, host-aware where
/// that matters. Cross-compiling setups will usually want to override with
/// `--cc`; these are starting points, not requirements.
pub fn default_driver(target: Target) -> String {
    let host = Target::host();

    match target.os {
        // macOS executables can realistically only be produced on a mac,
        // where cc (clang) builds either architecture with -arch — including
        // x86_64 binaries on Apple Silicon and back.
        Os::Macos => format!("cc -arch {}", target.arch.name()),

        Os::Linux => {
            if target == host {
                "cc".to_string()
            } else {
                // Names of the usual cross-compiler packages
                // (gcc-aarch64-linux-gnu and friends). `clang
                // --target=<triple> -fuse-ld=lld` works too — pass --cc.
                match target.arch {
                    Arch::Aarch64 => "aarch64-linux-gnu-gcc".to_string(),
                    Arch::X86_64 => "x86_64-linux-gnu-gcc".to_string(),
                }
            }
        }

        // Windows targets need a MinGW-family driver (Debian/Ubuntu cross
        // packages, MSYS2, or llvm-mingw). MSVC's cl can't consume
        // GNU-syntax assembly, so it's not a supported driver at all.
        Os::Windows => match target.arch {
            // Present in distro packages and in MSYS2's mingw-w64 toolchain.
            Arch::X86_64 => "x86_64-w64-mingw32-gcc".to_string(),
            // aarch64 mingw isn't packaged by distros; llvm-mingw is the
            // usual source.
            Arch::Aarch64 => "aarch64-w64-mingw32-gcc".to_string(),
        },
    }
}

/// Driver flags appended after the assembly file on the command line. They
/// encode each OS's idea of what "standalone" means:
///
/// - Linux: freestanding (raw syscalls, our own `_start`) and fully static,
///   so the result has zero dependencies of any kind.
/// - macOS: an ordinary cc link — `main` and libSystem's `read`/`write`.
///   A static/no-libc executable isn't a thing macOS offers.
/// - Windows: freestanding again (Win32 API for I/O, our own entry symbol
///   wired up with -e), linking nothing but kernel32.
pub fn driver_flags(target: Target) -> &'static [&'static str] {
    match target.os {
        Os::Linux => &["-nostdlib", "-static"],
        Os::Macos => &[],
        Os::Windows => &["-nostdlib", "-lkernel32", "-Wl,-e,bf_start"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_six_names() {
        for target in ALL_TARGETS {
            assert_eq!(Target::from_name(&target.name()), Ok(target));
        }
    }

    #[test]
    fn unknown_target_error_lists_valid_names() {
        let err = Target::from_name("linux-arm64").unwrap_err();
        assert!(err.contains("linux-aarch64"));
        assert!(err.contains("windows-aarch64"));
    }

    #[test]
    fn windows_entry_flag_is_wired_to_our_symbol() {
        assert!(
            driver_flags(Target::from_name("windows-x86_64").unwrap()).contains(&"-Wl,-e,bf_start")
        );
        assert!(
            !driver_flags(Target::from_name("linux-x86_64").unwrap()).contains(&"-Wl,-e,bf_start")
        );
    }
}
