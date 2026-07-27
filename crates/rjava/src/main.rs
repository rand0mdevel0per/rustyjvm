//! `java`-compatible launcher for RustyJVM.
//!
//! M1 scope: a placeholder entry point. Increment 1 wires class loading + interpretation of a
//! verified static method; later increments add a full `java`-compatible command line.

fn main() {
    println!(
        "RustyJVM {} (skeleton) — see specs/RJVM_SPEC_001.MD",
        rjava::VERSION
    );
}
