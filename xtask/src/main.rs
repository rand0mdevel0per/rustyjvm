//! xtask — build & benchmark driver for RustyJVM.
//!
//! `cargo xtask bench` (aliased as `cargo benchmark-test`) compiles the `testdata/java` fixtures
//! with javac, then times the pipeline stages (parse / verify / SSA build) and interpreter
//! throughput. javac/Corretto is used only to produce inputs; the timings are RustyJVM's.

use std::time::Instant;

use rjava_classfile::ClassFile;
use rjava_core::Val128;
use rjava_interp::Program;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("bench") => bench(),
        _ => eprintln!("usage: cargo xtask bench   (or: cargo benchmark-test)"),
    }
}

fn testdata_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../testdata/java")
}

/// Compile the named fixtures with `javac --release 21` into a temp dir.
fn compile(names: &[&str]) -> Option<std::path::PathBuf> {
    let src_dir = testdata_dir();
    let out = std::env::temp_dir().join(format!("rjava-bench-{}", std::process::id()));
    std::fs::create_dir_all(&out).ok()?;
    let mut cmd = std::process::Command::new("javac");
    cmd.args(["--release", "21", "-d"]).arg(&out);
    for n in names {
        cmd.arg(src_dir.join(format!("{n}.java")));
    }
    if cmd.status().ok()?.success() {
        Some(out)
    } else {
        None
    }
}

fn load(dir: &std::path::Path, name: &str) -> ClassFile {
    let bytes = std::fs::read(dir.join(format!("{name}.class"))).unwrap();
    rjava_classfile::parse(&bytes).unwrap()
}

/// Time `iters` executions of `f` (with a short warm-up) and print ns/op + ops/s.
fn timed(label: &str, iters: u64, mut f: impl FnMut()) {
    for _ in 0..(iters / 10).max(1) {
        f();
    }
    let t = Instant::now();
    for _ in 0..iters {
        f();
    }
    let ns = t.elapsed().as_nanos() as f64 / iters as f64;
    println!(
        "{label:<38} {iters:>9} iters  {ns:>12.1} ns/op  {:>13.0} ops/s",
        1e9 / ns
    );
}

fn bench() {
    let Some(dir) = compile(&["Slice", "Loops", "Calls", "Mixed"]) else {
        eprintln!("bench: javac (release 21) unavailable — cannot produce benchmark inputs");
        std::process::exit(2);
    };
    println!("RustyJVM micro-benchmarks (release build).\n");

    // --- pipeline stages on Slice.arith ---
    let slice_bytes = std::fs::read(dir.join("Slice.class")).unwrap();
    timed("parse Slice.class", 50_000, || {
        rjava_classfile::parse(&slice_bytes).unwrap();
    });
    let slice = load(&dir, "Slice");
    let arith = slice.method("arith", "(IIJF)I").unwrap();
    timed("verify Slice.arith", 50_000, || {
        rjava_verify::verify_method(&slice, arith).unwrap();
    });
    let vm = rjava_verify::verify_method(&slice, arith).unwrap();
    timed("build Slice.arith (stack->SSA)", 50_000, || {
        rjava_ir::build(&vm, &slice).unwrap();
    });
    let calls = load(&dir, "Calls");
    timed("Program::from_class Calls (7 methods)", 10_000, || {
        Program::from_class(&calls);
    });

    println!();

    // --- interpreter throughput ---
    let slice_prog = Program::from_class(&slice);
    let arith_args = [
        Val128::from_i32(50),
        Val128::from_i32(3),
        Val128::from_i64(100),
        Val128::from_f32(200.0),
    ];
    timed("exec Slice.arith() [straight-line]", 1_000_000, || {
        slice_prog
            .call_named("arith", "(IIJF)I", &arith_args)
            .unwrap();
    });
    let loops_prog = Program::from_class(&load(&dir, "Loops"));
    let big = [Val128::from_i32(100_000)];
    timed("exec Loops.sumTo(100000) [tight loop]", 2_000, || {
        loops_prog.call_named("sumTo", "(I)I", &big).unwrap();
    });
    let calls_prog = Program::from_class(&calls);
    let n28 = [Val128::from_i32(28)];
    timed("exec Calls.fib(28) [~832k calls]", 20, || {
        calls_prog.call_named("fib", "(I)I", &n28).unwrap();
    });
    let mixed_prog = Program::from_class(&load(&dir, "Mixed"));
    let n2000 = [Val128::from_i32(2000)];
    timed("exec Mixed.totient(2000) [loop+call]", 30, || {
        mixed_prog.call_named("totient", "(I)I", &n2000).unwrap();
    });
}
