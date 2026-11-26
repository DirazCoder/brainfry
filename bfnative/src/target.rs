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
        Target {
            arch: arch_from_flags(cfg!(target_arch = "x86_64"), cfg!(target_arch = "aarch64")),
            os: os_from_flags(
                cfg!(target_os = "linux"),
                cfg!(target_os = "macos"),
                cfg!(target_os = "windows"),
            ),
        }
    }
}

/// Pure decision function behind [`Target::host`]'s OS pick, taking the
/// cfg! booleans as arguments so every branch (including the "impossible
/// host" panics) is directly unit-testable instead of only ever taking the
/// one branch the current build host selects.
fn os_from_flags(is_linux: bool, is_macos: bool, is_windows: bool) -> Os {
    if is_linux {
        Os::Linux
    } else if is_macos {
        Os::Macos
    } else if is_windows {
        Os::Windows
    } else {
        panic!("bfnative doesn't know how to pick a default target on this host OS")
    }
}

/// Pure decision function behind [`Target::host`]'s arch pick, same
/// rationale as [`os_from_flags`].
fn arch_from_flags(is_x86_64: bool, is_aarch64: bool) -> Arch {
    if is_x86_64 {
        Arch::X86_64
    } else if is_aarch64 {
        Arch::Aarch64
    } else {
        panic!("bfnative doesn't know how to pick a default target on this host CPU")
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.os.name(), self.arch.name())
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
    fn os_from_flags_selects_every_supported_os() {
        assert_eq!(os_from_flags(true, false, false), Os::Linux);
        assert_eq!(os_from_flags(false, true, false), Os::Macos);
        assert_eq!(os_from_flags(false, false, true), Os::Windows);
    }

    #[test]
    #[should_panic(expected = "doesn't know how to pick a default target on this host OS")]
    fn os_from_flags_panics_on_an_unknown_host() {
        os_from_flags(false, false, false);
    }

    #[test]
    fn arch_from_flags_selects_both_architectures() {
        assert_eq!(arch_from_flags(true, false), Arch::X86_64);
        assert_eq!(arch_from_flags(false, true), Arch::Aarch64);
    }

    #[test]
    #[should_panic(expected = "doesn't know how to pick a default target on this host CPU")]
    fn arch_from_flags_panics_on_an_unknown_host() {
        arch_from_flags(false, false);
    }

    #[test]
    fn host_agrees_with_the_build_cfgs() {
        // host() is a thin wrapper over the two pure selectors fed the real
        // cfg! values, so on any supported build host it must land on one of
        // the six known targets.
        assert!(ALL_TARGETS.contains(&Target::host()));
    }
}
