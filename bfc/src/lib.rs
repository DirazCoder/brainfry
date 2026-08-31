//! The compiler's library half: parsing and optimization over the shared
//! `bfformat` op list. The `bfc` binary is a thin CLI over these passes, and
//! `bfnative` reuses them so the bytecode and native backends run the exact
//! same front end instead of drifting apart.

pub mod optimize;
pub mod parser;
