//! The compiler's library half: parsing and optimization over the shared
//! `bfformat` op list. The `bfc` binary is a thin CLI over these passes, and
//! `bfnative` reuses them so the bytecode and native backends run the exact
//! same front end instead of drifting apart.

// Enables the unstable `coverage(off)` attribute for nightly coverage runs
// only (cargo-llvm-cov sets `coverage_nightly`); plain builds, stable or
// nightly, never see it. See the note on fold_offset_adds for why it's used.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod optimize;
pub mod parser;
