//! Shared native code generation used by the executable writer and JIT.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod codegen;
pub mod target;
