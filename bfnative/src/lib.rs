//! Shared native code generation used by the executable writer and JIT.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
pub mod cli;
pub mod codegen;
mod listing;
mod obj;
mod sha256;
pub mod target;
