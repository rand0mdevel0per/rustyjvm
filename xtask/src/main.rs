//! xtask — build & differential-test driver for RustyJVM.
//!
//! Invoked as `cargo xtask <cmd>`. M1 will grow a `diff-test` subcommand that compiles
//! `testdata/java/*.java` with `javac` and compares RustyJVM output against a Corretto 21
//! reflective oracle (RJVM-SPEC-001 §23.1). Skeleton: prints usage.

fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    match cmd.as_str() {
        "diff-test" => {
            eprintln!("xtask diff-test: not yet implemented (arrives in increment 1)");
            std::process::exit(2);
        }
        _ => {
            eprintln!("usage: cargo xtask <diff-test>");
        }
    }
}
