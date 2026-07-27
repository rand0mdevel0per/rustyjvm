//! rjava — top-level embeddable RustyJVM library + `java`-compatible launcher.
//!
//! North star: observable-behavior conformance to Amazon Corretto 21 (RJVM-SPEC-001 §P-5).
//! This crate re-exports the engine crates and hosts the CLI entry point. It is intentionally
//! thin: all correctness-critical logic lives in the interpreter (P-1).

/// Semantic version of the RustyJVM runtime (STD-VER-1).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
