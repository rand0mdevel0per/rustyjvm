//! rjava-interp — the L3 register interpreter (RJVM-SPEC-001 §10.1). Increment 1 executes an L1
//! SSA method directly over a single [`Env`] (register file): each `Node` reads its `ins` slots,
//! computes, and writes its own slot; `Terminator`s drive control flow. This is the correct
//! baseline executor per P-1 — no speculation, no fork, no heap yet. The `match` on `Op` is the
//! computed-jump dispatch; the diff/fork machinery and the `L2Op.accel` JIT seam attach in
//! increments 4+ without changing this loop.
//!
//! IEEE-754 is strict (§13.6): Rust's wrapping integer ops and saturating float→int `as` casts
//! match the JVM's `iadd`/`ldiv`/`f2i`/… semantics exactly, which is what makes the differential
//! results bit-identical to Corretto.

use rjava_core::{
    BlockId, ClassId, Effect, Env, EscapeState, IntCond, LogicalFrame, Op, RefIndex, SlotId, Tag,
    Terminator, Val128,
};
use rjava_gc::Heap;
use rjava_ir::BuiltMethod;

/// A runtime failure. Increment 1 cannot yet raise Java exceptions (those arrive in increment 8);
/// conditions like division by zero surface as errors here for now.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExecError {
    #[error("division by zero")]
    DivByZero,
    #[error("value/type error during execution")]
    BadValue,
    #[error("operation not supported by the increment-1 interpreter")]
    UnsupportedOp,
    #[error("exception thrown (implemented in increment 8)")]
    Thrown,
    #[error("execution step limit exceeded")]
    StepLimit,
    #[error("control transfer to a nonexistent block")]
    BadBlock,
    #[error("argument count mismatch")]
    BadArgs,
    #[error("phi has no operand for the incoming control-flow edge")]
    BadPhi,
    #[error("call stack depth exceeded (StackOverflowError)")]
    StackOverflow,
    #[error("no such method, or the callee failed to build")]
    NoMethod,
    #[error("out of memory (allocation failed, §22.1)")]
    OutOfMemory,
    #[error("null pointer (NullPointerException, implemented in increment 8)")]
    NullPointer,
}

/// Default guest call-stack limit, in frames.
///
/// The interpreter currently recurses on the **host** stack for each Java call, so this bound must
/// keep the deepest guest recursion inside the host thread's stack. Measured worst case (a debug
/// build on a default 2 MiB thread stack) is ~4 KiB of host stack per frame: 400 frames survive and
/// 600 overflow, so the default leaves roughly a 1.5× margin. Without such a bound, deep guest
/// recursion aborts the process with a host stack overflow instead of raising a Java
/// `StackOverflowError` — silently trading a catchable error for a crash.
///
/// An embedder that runs guest code on a larger stack can raise this via
/// [`Program::with_max_call_depth`]. The real fix is an explicit frame stack (an iterative
/// interpreter), which increment 8 needs anyway for stack traces (§20.7).
pub const DEFAULT_MAX_CALL_DEPTH: usize = 256;

/// A built class: every static method built once (unbuildable ones → `None`), plus a name→index
/// map. `invokestatic` resolves against this table and recurses into the callee. Cross-class
/// dispatch (via the loader, §8) arrives in increment 6.
pub struct Program {
    methods: Vec<Option<BuiltMethod>>,
    by_name: std::collections::HashMap<(String, String), u16>,
    /// Type-appropriate zero values for this class's instance fields, in declaration order (JVMS
    /// default-value semantics: int→0, long→0L, float→0.0, double→0.0, reference→null).
    field_defaults: Vec<Val128>,
    /// Guest call-stack limit for this program (see [`DEFAULT_MAX_CALL_DEPTH`]).
    max_call_depth: usize,
}

/// The JVMS default value for a field of the given descriptor.
fn field_default(desc: Option<&str>) -> Val128 {
    match desc.and_then(|d| d.as_bytes().first()) {
        Some(b'J') => Val128::from_i64(0),
        Some(b'F') => Val128::from_f32(0.0),
        Some(b'D') => Val128::from_f64(0.0),
        Some(b'L') | Some(b'[') => Val128::null(),
        _ => Val128::from_i32(0), // B, C, I, S, Z
    }
}

impl Program {
    /// Verify + build every method of a class.
    pub fn from_class(cf: &rjava_classfile::ClassFile) -> Program {
        let mut by_name = std::collections::HashMap::new();
        let methods = cf
            .methods
            .iter()
            .enumerate()
            .map(|(i, m)| {
                if let (Some(n), Some(d)) =
                    (m.name(&cf.constant_pool), m.descriptor(&cf.constant_pool))
                {
                    by_name.insert((n.to_string(), d.to_string()), i as u16);
                }
                rjava_verify::verify_method(cf, m)
                    .ok()
                    .and_then(|vm| rjava_ir::build(&vm, cf).ok())
            })
            .collect();
        let field_defaults = cf
            .fields
            .iter()
            .filter(|m| m.access_flags & 0x0008 == 0) // instance (non-static) fields
            .map(|m| field_default(m.descriptor(&cf.constant_pool)))
            .collect();
        Program {
            methods,
            by_name,
            field_defaults,
            max_call_depth: DEFAULT_MAX_CALL_DEPTH,
        }
    }

    /// Raise (or lower) the guest call-stack limit — only sound if guest code runs on a host thread
    /// whose stack can hold that many frames (see [`DEFAULT_MAX_CALL_DEPTH`]).
    pub fn with_max_call_depth(mut self, frames: usize) -> Self {
        self.max_call_depth = frames;
        self
    }

    /// Invoke a method by its class-table index; `None` = a `void` return.
    pub fn call(
        &self,
        index: u16,
        args: &[Val128],
        depth: usize,
        heap: &mut Heap,
    ) -> Result<Option<Val128>, ExecError> {
        let built = self
            .methods
            .get(index as usize)
            .and_then(Option::as_ref)
            .ok_or(ExecError::NoMethod)?;
        run(
            Some(self),
            built,
            args,
            depth,
            LogicalFrame(index as u32),
            heap,
        )
    }

    /// The class-table index of a method, by name and descriptor.
    pub fn method_index(&self, name: &str, desc: &str) -> Option<u16> {
        self.by_name
            .get(&(name.to_string(), desc.to_string()))
            .copied()
    }

    /// Invoke a method by name + descriptor (the launcher/test entry point).
    pub fn call_named(
        &self,
        name: &str,
        desc: &str,
        args: &[Val128],
    ) -> Result<Option<Val128>, ExecError> {
        let idx = *self
            .by_name
            .get(&(name.to_string(), desc.to_string()))
            .ok_or(ExecError::NoMethod)?;
        let mut heap = Heap::new();
        self.call(idx, args, 0, &mut heap)
    }
}

/// Execute a call-free built method (increment 1/2a entry point).
pub fn execute(built: &BuiltMethod, args: &[Val128]) -> Result<Val128, ExecError> {
    let mut heap = Heap::new();
    run(None, built, args, 0, LogicalFrame(0), &mut heap)?.ok_or(ExecError::BadValue)
}

/// Land a chain: apply its recorded reference-count deltas to the heap in program order, then
/// commit its slot writes (§5.5, §10.5). `buf` is reused across landings to avoid allocating.
#[inline]
fn land_chain(env: &mut Env, heap: &mut Heap, buf: &mut Vec<(RefIndex, i32)>) {
    env.drain_ref_deltas(buf);
    if !buf.is_empty() {
        heap.apply_rc_deltas(buf);
    }
    env.land();
}

/// Leave a frame: land whatever is still pending, then release the references its slots held.
///
/// The returned reference is handed to the caller as an *in-transit* reference (+1) inside the same
/// batch that releases the slots, so it cannot be reclaimed in between; the caller drops that
/// in-transit reference once a slot of its own owns it. Scope-exclusive (S1) objects were never
/// counted at all — they are reclaimed here by RAII (§5.1), which also releases anything their
/// fields referenced.
fn leave_frame(
    env: &mut Env,
    heap: &mut Heap,
    buf: &mut Vec<(RefIndex, i32)>,
    n_slots: usize,
    returned: Option<Val128>,
) {
    land_chain(env, heap, buf);

    buf.clear();
    if let Some(v) = returned {
        if v.tag().is_ref() {
            buf.push((v.ref_index(), 1)); // in-transit reference for the caller
        }
    }
    for i in 0..n_slots {
        let v = env.read_slot(SlotId(i as u16));
        if v.tag() == Tag::Ptr {
            buf.push((v.ref_index(), -1));
        }
    }
    if !buf.is_empty() {
        heap.apply_rc_deltas(buf);
    }
    // S1 handles die with the scope that owned them (§5.1). A handle may sit in several slots
    // (a φ merge); reclaiming is idempotent, so the repeat is harmless.
    for i in 0..n_slots {
        let v = env.read_slot(SlotId(i as u16));
        if v.tag() == Tag::Handle {
            heap.free(v.ref_index());
        }
    }
}

/// Execute a built method over a fresh Env register file. `program` is needed only if the method
/// contains `invokestatic`; `depth`/`logical` thread the call stack (StackOverflow seam + logical
/// frames, §20.7).
fn run(
    program: Option<&Program>,
    built: &BuiltMethod,
    args: &[Val128],
    depth: usize,
    logical: LogicalFrame,
    heap: &mut Heap,
) -> Result<Option<Val128>, ExecError> {
    let limit = program.map_or(DEFAULT_MAX_CALL_DEPTH, |p| p.max_call_depth);
    if depth > limit {
        return Err(ExecError::StackOverflow);
    }
    if args.len() != built.arg_vals.len() {
        return Err(ExecError::BadArgs);
    }
    let mut env = Env::new(built.n_slots.max(1), logical);
    for (slot, &value) in built.arg_vals.iter().zip(args) {
        env.write_slot(SlotId(slot.offset), value);
    }

    let mut cur = built.method.entry;
    let mut prev: Option<BlockId> = None;
    let mut steps: u64 = 0;
    // Reused across landings so the reference-count batches cost no allocation.
    let mut rc_buf: Vec<(RefIndex, i32)> = Vec::new();
    let n_slots = built.n_slots.max(1);
    loop {
        steps += 1;
        if steps > 100_000_000 {
            return Err(ExecError::StepLimit);
        }
        let block = built
            .method
            .blocks
            .get(cur.0 as usize)
            .ok_or(ExecError::BadBlock)?;

        // φ resolution: read every source for the edge we arrived on, then write all φ slots
        // (parallel-copy semantics so self-referential loop φ behave correctly).
        if let Some(p) = prev {
            if !block.phis.is_empty() {
                let mut writes: smallvec::SmallVec<[(u16, Val128); 4]> = smallvec::SmallVec::new();
                for phi in &block.phis {
                    let (_, v) = phi
                        .sources
                        .iter()
                        .find(|(pb, _)| *pb == p)
                        .ok_or(ExecError::BadPhi)?;
                    writes.push((phi.slot.offset, env.read_slot(SlotId(v.offset))));
                }
                for (slot, val) in writes {
                    env.write_slot(SlotId(slot), val);
                }
            }
        }

        for node in &block.nodes {
            // `Effect::Extern` is a fence: it is never speculated and its predecessors land in
            // program order before it runs (§10.6). Calls carry this effect.
            if node.effect == Effect::Extern {
                land_chain(&mut env, heap, &mut rc_buf);
            }
            match node.op {
                // Calls (need the Program + recursion + shared heap).
                Op::InvokeStatic(index) | Op::InvokeSpecial(index) => {
                    let prog = program.ok_or(ExecError::UnsupportedOp)?;
                    let mut argv: smallvec::SmallVec<[Val128; 4]> = smallvec::SmallVec::new();
                    for v in &node.ins {
                        argv.push(env.read_slot(SlotId(v.offset)));
                    }
                    if let Some(r) = prog.call(index, &argv, depth + 1, heap)? {
                        env.write_slot(SlotId(node.id.offset), r);
                        // The callee handed over an in-transit reference; now that a slot of ours
                        // owns it, drop that extra reference (both deltas land together).
                        if r.tag().is_ref() {
                            env.record_ref_delta(r.ref_index(), -1);
                        }
                    }
                }
                // Heap operations.
                Op::New { escape, .. } => {
                    let prog = program.ok_or(ExecError::UnsupportedOp)?;
                    let r = heap
                        .alloc(ClassId(0), escape, prog.field_defaults.clone())
                        .ok_or(ExecError::OutOfMemory)?;
                    let v = if escape == EscapeState::S1 {
                        Val128::handle(r)
                    } else {
                        Val128::ptr(r)
                    };
                    env.write_slot(SlotId(node.id.offset), v);
                }
                Op::GetField(idx) => {
                    let obj =
                        env.read_slot(SlotId(node.ins.first().ok_or(ExecError::BadValue)?.offset));
                    if obj.tag() == Tag::Null {
                        return Err(ExecError::NullPointer);
                    }
                    let v = heap
                        .get_field(obj.ref_index(), idx as usize)
                        .ok_or(ExecError::BadValue)?;
                    env.write_slot(SlotId(node.id.offset), v);
                }
                Op::PutField(idx) => {
                    let obj =
                        env.read_slot(SlotId(node.ins.first().ok_or(ExecError::BadValue)?.offset));
                    let val =
                        env.read_slot(SlotId(node.ins.get(1).ok_or(ExecError::BadValue)?.offset));
                    if obj.tag() == Tag::Null {
                        return Err(ExecError::NullPointer);
                    }
                    // A field is a strong reference too: the overwritten referent loses one and the
                    // stored one gains it. Recorded now, applied when the chain lands (§5.5).
                    let old = heap
                        .get_field(obj.ref_index(), idx as usize)
                        .ok_or(ExecError::BadValue)?;
                    if !heap.set_field(obj.ref_index(), idx as usize, val) {
                        return Err(ExecError::BadValue);
                    }
                    if val.tag().is_ref() {
                        env.record_ref_delta(val.ref_index(), 1);
                    }
                    if old.tag().is_ref() {
                        env.record_ref_delta(old.ref_index(), -1);
                    }
                }
                // Pure computations.
                _ => {
                    let mut inputs: smallvec::SmallVec<[Val128; 3]> = smallvec::SmallVec::new();
                    for v in &node.ins {
                        inputs.push(env.read_slot(SlotId(v.offset)));
                    }
                    let result = eval(node.op, node.ty, &inputs)?;
                    env.write_slot(SlotId(node.id.offset), result);
                }
            }
        }

        match &block.term {
            Terminator::Return(Some(v)) => {
                let rv = env.read_slot(SlotId(v.offset));
                leave_frame(&mut env, heap, &mut rc_buf, n_slots, Some(rv));
                return Ok(Some(rv));
            }
            Terminator::Return(None) => {
                leave_frame(&mut env, heap, &mut rc_buf, n_slots, None);
                return Ok(None);
            }
            Terminator::Goto(b) => {
                prev = Some(cur);
                cur = *b;
            }
            Terminator::CondBranch {
                cond,
                taken,
                not_taken,
            } => {
                let c = env.read_slot(SlotId(cond.offset)).as_i32();
                prev = Some(cur);
                cur = if c != 0 { *taken } else { *not_taken };
            }
            Terminator::Throw(_) => return Err(ExecError::Thrown),
        }
        // End of chain: land this block's diff into the environment in program order (§10.5).
        // Landing incrementally — rather than batching at scope exit — is what keeps intra-vt
        // execution as-if-serial and makes the increment-8 po-truncation on a caught throw correct.
        land_chain(&mut env, heap, &mut rc_buf);
    }
}

/// Evaluate a single SSA node.
fn eval(op: Op, ty: Tag, ins: &[Val128]) -> Result<Val128, ExecError> {
    Ok(match op {
        Op::Const(v) => v,
        Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Rem => {
            let a = *ins.first().ok_or(ExecError::BadValue)?;
            let b = *ins.get(1).ok_or(ExecError::BadValue)?;
            binary(op, ty, a, b)?
        }
        Op::Neg => {
            let a = *ins.first().ok_or(ExecError::BadValue)?;
            match ty {
                Tag::I32 => Val128::from_i32(a.as_i32().wrapping_neg()),
                Tag::I64 => Val128::from_i64(a.as_i64().wrapping_neg()),
                Tag::F32 => Val128::from_f32(-a.as_f32()),
                Tag::F64 => Val128::from_f64(-a.as_f64()),
                _ => return Err(ExecError::UnsupportedOp),
            }
        }
        Op::Convert => convert(*ins.first().ok_or(ExecError::BadValue)?, ty)?,
        Op::Cmp { nan_greater } => {
            let a = *ins.first().ok_or(ExecError::BadValue)?;
            let b = *ins.get(1).ok_or(ExecError::BadValue)?;
            cmp3(a, b, nan_greater)?
        }
        Op::TestZero(cond) => {
            let v = ins.first().ok_or(ExecError::BadValue)?.as_i32();
            let taken = match cond {
                IntCond::Eq => v == 0,
                IntCond::Ne => v != 0,
                IntCond::Lt => v < 0,
                IntCond::Ge => v >= 0,
                IntCond::Gt => v > 0,
                IntCond::Le => v <= 0,
            };
            Val128::from_i32(taken as i32)
        }
        Op::TestNull { expect_null } => {
            let is_null = ins.first().ok_or(ExecError::BadValue)?.tag() == Tag::Null;
            Val128::from_i32((is_null == expect_null) as i32)
        }
        Op::ICmp(cond) => {
            let a = ins.first().ok_or(ExecError::BadValue)?.as_i32();
            let b = ins.get(1).ok_or(ExecError::BadValue)?.as_i32();
            let taken = match cond {
                IntCond::Eq => a == b,
                IntCond::Ne => a != b,
                IntCond::Lt => a < b,
                IntCond::Ge => a >= b,
                IntCond::Gt => a > b,
                IntCond::Le => a <= b,
            };
            Val128::from_i32(taken as i32)
        }
        Op::And => {
            let a = *ins.first().ok_or(ExecError::BadValue)?;
            let b = *ins.get(1).ok_or(ExecError::BadValue)?;
            match ty {
                Tag::I32 => Val128::from_i32(a.as_i32() & b.as_i32()),
                Tag::I64 => Val128::from_i64(a.as_i64() & b.as_i64()),
                _ => return Err(ExecError::UnsupportedOp),
            }
        }
        // Calls and heap ops are dispatched in `run` (they need the Program/heap), never here.
        Op::InvokeStatic(_)
        | Op::InvokeSpecial(_)
        | Op::New { .. }
        | Op::GetField(_)
        | Op::PutField(_) => return Err(ExecError::UnsupportedOp),
    })
}

/// Typed binary arithmetic (add/sub/mul/div/rem). Integer div/rem trap on a zero divisor and wrap
/// on `MIN / -1`; floats follow strict IEEE-754.
fn binary(op: Op, ty: Tag, a: Val128, b: Val128) -> Result<Val128, ExecError> {
    Ok(match ty {
        Tag::I32 => {
            let (x, y) = (a.as_i32(), b.as_i32());
            Val128::from_i32(match op {
                Op::Add => x.wrapping_add(y),
                Op::Sub => x.wrapping_sub(y),
                Op::Mul => x.wrapping_mul(y),
                Op::Div => {
                    if y == 0 {
                        return Err(ExecError::DivByZero);
                    }
                    x.wrapping_div(y)
                }
                Op::Rem => {
                    if y == 0 {
                        return Err(ExecError::DivByZero);
                    }
                    x.wrapping_rem(y)
                }
                _ => return Err(ExecError::UnsupportedOp),
            })
        }
        Tag::I64 => {
            let (x, y) = (a.as_i64(), b.as_i64());
            Val128::from_i64(match op {
                Op::Add => x.wrapping_add(y),
                Op::Sub => x.wrapping_sub(y),
                Op::Mul => x.wrapping_mul(y),
                Op::Div => {
                    if y == 0 {
                        return Err(ExecError::DivByZero);
                    }
                    x.wrapping_div(y)
                }
                Op::Rem => {
                    if y == 0 {
                        return Err(ExecError::DivByZero);
                    }
                    x.wrapping_rem(y)
                }
                _ => return Err(ExecError::UnsupportedOp),
            })
        }
        Tag::F32 => {
            let (x, y) = (a.as_f32(), b.as_f32());
            Val128::from_f32(match op {
                Op::Add => x + y,
                Op::Sub => x - y,
                Op::Mul => x * y,
                Op::Div => x / y,
                Op::Rem => x % y,
                _ => return Err(ExecError::UnsupportedOp),
            })
        }
        Tag::F64 => {
            let (x, y) = (a.as_f64(), b.as_f64());
            Val128::from_f64(match op {
                Op::Add => x + y,
                Op::Sub => x - y,
                Op::Mul => x * y,
                Op::Div => x / y,
                Op::Rem => x % y,
                _ => return Err(ExecError::UnsupportedOp),
            })
        }
        _ => return Err(ExecError::UnsupportedOp),
    })
}

/// Numeric conversion driven by the source value's tag and the target type. Rust's `as` casts
/// match the JVM: int->float rounds to nearest; float->int saturates (NaN->0, out-of-range clamps),
/// exactly the `f2i`/`f2l`/`d2i`/`d2l` semantics.
fn convert(a: Val128, ty: Tag) -> Result<Val128, ExecError> {
    use Tag::{F32, F64, I32, I64};
    Ok(match (a.tag(), ty) {
        (I32, I64) => Val128::from_i64(a.as_i32() as i64),
        (I32, F32) => Val128::from_f32(a.as_i32() as f32),
        (I32, F64) => Val128::from_f64(a.as_i32() as f64),
        (I64, I32) => Val128::from_i32(a.as_i64() as i32),
        (I64, F32) => Val128::from_f32(a.as_i64() as f32),
        (I64, F64) => Val128::from_f64(a.as_i64() as f64),
        (F32, I32) => Val128::from_i32(a.as_f32() as i32),
        (F32, I64) => Val128::from_i64(a.as_f32() as i64),
        (F32, F64) => Val128::from_f64(a.as_f32() as f64),
        (F64, I32) => Val128::from_i32(a.as_f64() as i32),
        (F64, I64) => Val128::from_i64(a.as_f64() as i64),
        (F64, F32) => Val128::from_f32(a.as_f64() as f32),
        _ => return Err(ExecError::UnsupportedOp),
    })
}

/// Three-way compare (`lcmp`/`fcmp<op>`): -1/0/1. On a float NaN, `fcmpg` yields 1 and `fcmpl`
/// yields -1 (`nan_greater` selects which).
fn cmp3(a: Val128, b: Val128, nan_greater: bool) -> Result<Val128, ExecError> {
    let r = match a.tag() {
        Tag::I64 => {
            let (x, y) = (a.as_i64(), b.as_i64());
            (x > y) as i32 - (x < y) as i32
        }
        Tag::F32 => float_cmp(a.as_f32() as f64, b.as_f32() as f64, nan_greater),
        Tag::F64 => float_cmp(a.as_f64(), b.as_f64(), nan_greater),
        _ => return Err(ExecError::UnsupportedOp),
    };
    Ok(Val128::from_i32(r))
}

fn float_cmp(x: f64, y: f64, nan_greater: bool) -> i32 {
    if x.is_nan() || y.is_nan() {
        if nan_greater {
            1
        } else {
            -1
        }
    } else if x > y {
        1
    } else if x < y {
        -1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rjava_ir::build;

    fn compile_slice() -> Option<Vec<u8>> {
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java/Slice.java");
        if !src.exists() {
            return None;
        }
        let out = std::env::temp_dir().join(format!("rjava-ix-{}", std::process::id()));
        std::fs::create_dir_all(&out).ok()?;
        let ok = std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&out)
            .arg(&src)
            .status()
            .ok()?
            .success();
        if !ok {
            return None;
        }
        std::fs::read(out.join("Slice.class")).ok()
    }

    #[test]
    fn executes_slice_arith_matches_hand_computed() {
        let Some(bytes) = compile_slice() else {
            eprintln!("skipping executes_slice_arith: javac unavailable");
            return;
        };
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let arith = cf.method("arith", "(IIJF)I").unwrap();
        let vm = rjava_verify::verify_method(&cf, arith).unwrap();
        let built = build(&vm, &cf).unwrap();

        let run = |a: i32, b: i32, c: i64, d: f32| -> i32 {
            let args = [
                Val128::from_i32(a),
                Val128::from_i32(b),
                Val128::from_i64(c),
                Val128::from_f32(d),
            ];
            execute(&built, &args).unwrap().as_i32()
        };

        // Hand-computed oracles across all three return paths of `arith`.
        assert_eq!(run(1, 2, 3, 4.0), -7); // else: -s
        assert_eq!(run(50, 3, 100, 200.0), -53); // first: (int)(t-(long)u)+s
        assert_eq!(run(2, 0, 0, 0.0), 0); // else-if: s*s (s==0)
    }

    fn tool_ok(name: &str) -> bool {
        std::process::Command::new(name)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// A diverse input set for `arith`: boundary values, NaN/Inf/subnormals, and deterministic
    /// pseudo-random tuples. `b == -1` is excluded so `a / (b + 1)` never divides by zero (integer
    /// division traps arrive in increment 8).
    fn gen_inputs() -> Vec<(i32, i32, i64, f32)> {
        let ints = [
            0i32,
            1,
            -1,
            2,
            -2,
            3,
            -3,
            7,
            -7,
            100,
            -100,
            46340,
            -46340,
            i32::MIN,
            i32::MAX,
            i32::MIN + 1,
            i32::MAX - 1,
        ];
        let longs = [
            0i64,
            1,
            -1,
            2,
            -2,
            100,
            -100,
            1 << 40,
            -(1 << 40),
            i64::MIN,
            i64::MAX,
        ];
        let floats = [
            0.0f32,
            -0.0,
            1.5,
            -3.25,
            100.5,
            999.9,
            1000.0,
            1e30,
            -1e30,
            0.1,
            f32::MIN_POSITIVE,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ];
        let mut out = Vec::new();
        for &a in &ints {
            for &b in &ints {
                if b == -1 {
                    continue;
                }
                out.push((a, b, 100, 200.0));
                out.push((a, b, i64::MAX, f32::NAN));
            }
        }
        for &c in &longs {
            for &d in &floats {
                out.push((50, 3, c, d));
                out.push((1, 2, c, d));
            }
        }
        // Deterministic LCG (no rand dependency); vary widely across the whole domain.
        let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            s
        };
        for _ in 0..400 {
            let a = next() as i32;
            let mut b = next() as i32;
            if b == -1 {
                b = 0;
            }
            let c = next() as i64;
            let d = f32::from_bits(next() as u32);
            out.push((a, b, c, d));
        }
        out
    }

    /// Run the Corretto oracle: reconstruct each float from its raw bits, invoke `Slice.arith`, and
    /// return one result per input line.
    fn run_oracle(dir: &std::path::Path, inputs: &[(i32, i32, i64, f32)]) -> Option<Vec<i32>> {
        use std::io::Write;
        let mut stdin_data = String::new();
        for &(a, b, c, d) in inputs {
            use std::fmt::Write as _;
            writeln!(stdin_data, "{} {} {} {}", a, b, c, d.to_bits() as i32).unwrap();
        }
        let mut child = std::process::Command::new("java")
            .arg("-cp")
            .arg(dir)
            .arg("Oracle")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .ok()?;
        child.stdin.take()?.write_all(stdin_data.as_bytes()).ok()?;
        let out = child.wait_with_output().ok()?;
        if !out.status.success() {
            eprintln!("oracle failed: {}", String::from_utf8_lossy(&out.stderr));
            return None;
        }
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().parse::<i32>().ok())
            .collect()
    }

    /// The increment-1 conformance gate (§P-5, §23.1): RustyJVM's `arith` result MUST equal
    /// Corretto 21's for every input. Skips when the JDK is absent (runs in the CI `differential`
    /// job, which provisions Corretto 21).
    #[test]
    fn differential_vs_corretto_21() {
        if !tool_ok("javac") || !tool_ok("java") {
            eprintln!("skipping differential_vs_corretto_21: JDK unavailable");
            return;
        }
        let slice_src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java/Slice.java");
        let dir = std::env::temp_dir().join(format!("rjava-diff-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // A tiny reflective oracle driver, compiled together with Slice.
        let oracle = r#"
import java.io.*;
public final class Oracle {
    public static void main(String[] a) throws Exception {
        var m = Slice.class.getMethod("arith", int.class, int.class, long.class, float.class);
        var r = new BufferedReader(new InputStreamReader(System.in));
        var sb = new StringBuilder();
        String line;
        while ((line = r.readLine()) != null) {
            if (line.isEmpty()) continue;
            String[] p = line.split("\\s+");
            int ia = Integer.parseInt(p[0]), ib = Integer.parseInt(p[1]);
            long c = Long.parseLong(p[2]);
            float d = Float.intBitsToFloat(Integer.parseInt(p[3]));
            sb.append((int) m.invoke(null, ia, ib, c, d)).append('\n');
        }
        System.out.print(sb);
    }
}
"#;
        std::fs::write(dir.join("Oracle.java"), oracle).unwrap();
        let compiled = std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&slice_src)
            .arg(dir.join("Oracle.java"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !compiled {
            // A default JDK is present but cannot target release 21 (e.g. CI's build-test job
            // without setup-java). The Corretto-21 `differential` CI job runs the real gate.
            eprintln!("skipping differential_vs_corretto_21: javac cannot target --release 21");
            return;
        }

        // RustyJVM pipeline.
        let bytes = std::fs::read(dir.join("Slice.class")).unwrap();
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let arith = cf.method("arith", "(IIJF)I").unwrap();
        let vm = rjava_verify::verify_method(&cf, arith).unwrap();
        let built = build(&vm, &cf).unwrap();

        let inputs = gen_inputs();
        let Some(oracle_results) = run_oracle(&dir, &inputs) else {
            eprintln!("skipping differential_vs_corretto_21: could not run the Corretto oracle");
            return;
        };
        assert_eq!(
            oracle_results.len(),
            inputs.len(),
            "oracle produced one result per input"
        );

        let mut mismatches = 0;
        for (&(a, b, c, d), &expected) in inputs.iter().zip(&oracle_results) {
            let args = [
                Val128::from_i32(a),
                Val128::from_i32(b),
                Val128::from_i64(c),
                Val128::from_f32(d),
            ];
            let got = execute(&built, &args).unwrap().as_i32();
            if got != expected {
                if mismatches < 10 {
                    eprintln!(
                        "MISMATCH arith({a}, {b}, {c}L, {}f/{:#010x}) = rjvm {got} vs corretto {expected}",
                        d, d.to_bits()
                    );
                }
                mismatches += 1;
            }
        }
        assert_eq!(
            mismatches,
            0,
            "{mismatches}/{} inputs diverged from Corretto 21",
            inputs.len()
        );
        eprintln!("differential OK: {} inputs match Corretto 21", inputs.len());
    }

    fn run_loops_oracle(dir: &std::path::Path, lines: &[String]) -> Option<Vec<i64>> {
        use std::io::Write;
        let stdin_data = lines.join("\n");
        let mut child = std::process::Command::new("java")
            .arg("-cp")
            .arg(dir)
            .arg("LoopsOracle")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .ok()?;
        child.stdin.take()?.write_all(stdin_data.as_bytes()).ok()?;
        let out = child.wait_with_output().ok()?;
        if !out.status.success() {
            eprintln!(
                "loops oracle failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return None;
        }
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().parse::<i64>().ok())
            .collect()
    }

    /// Increment-2 conformance gate: loops (real φ), `iinc`, `if_icmp`, `iand`, `lreturn` — every
    /// result must match Corretto 21. Inputs are bounded per method so the loops terminate.
    #[test]
    fn differential_loops_vs_corretto_21() {
        if !tool_ok("javac") || !tool_ok("java") {
            eprintln!("skipping differential_loops: JDK unavailable");
            return;
        }
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java/Loops.java");
        let dir = std::env::temp_dir().join(format!("rjava-loops-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let oracle = r#"
import java.io.*;
public final class LoopsOracle {
    public static void main(String[] x) throws Exception {
        var br = new BufferedReader(new InputStreamReader(System.in));
        var sb = new StringBuilder();
        String line;
        while ((line = br.readLine()) != null) {
            if (line.isEmpty()) continue;
            String[] p = line.split("\\s+");
            int a = Integer.parseInt(p[1]);
            int b = p.length > 2 ? Integer.parseInt(p[2]) : 0;
            long r;
            switch (p[0]) {
                case "sumTo": r = Loops.sumTo(a); break;
                case "factorial": r = Loops.factorial(a); break;
                case "fib": r = Loops.fib(a); break;
                case "gcd": r = Loops.gcd(a, b); break;
                case "collatz": r = Loops.collatz(a); break;
                default: r = 0;
            }
            sb.append(r).append('\n');
        }
        System.out.print(sb);
    }
}
"#;
        std::fs::write(dir.join("LoopsOracle.java"), oracle).unwrap();
        let compiled = std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&src)
            .arg(dir.join("LoopsOracle.java"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !compiled {
            eprintln!("skipping differential_loops: javac cannot target --release 21");
            return;
        }

        let bytes = std::fs::read(dir.join("Loops.class")).unwrap();
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let build_m = |name: &str, desc: &str| {
            let m = cf.method(name, desc).unwrap();
            let vm = rjava_verify::verify_method(&cf, m).unwrap();
            build(&vm, &cf).unwrap()
        };
        let sumto = build_m("sumTo", "(I)I");
        let factorial = build_m("factorial", "(I)J");
        let fib = build_m("fib", "(I)I");
        let gcd = build_m("gcd", "(II)I");
        let collatz = build_m("collatz", "(I)I");

        // (method, a, b, built, result_is_long)
        let mut cases: Vec<(&str, i32, i32, &BuiltMethod, bool)> = Vec::new();
        for n in [-5, -1, 0, 1, 2, 3, 5, 10, 100, 1000, 10000, 46340] {
            cases.push(("sumTo", n, 0, &sumto, false));
        }
        for n in [-1, 0, 1, 2, 3, 5, 10, 13, 20, 21, 25, 40] {
            cases.push(("factorial", n, 0, &factorial, true));
        }
        for n in [0, 1, 2, 3, 5, 10, 20, 45, 46, 47, 90, 92] {
            cases.push(("fib", n, 0, &fib, false));
        }
        for (a, b) in [
            (48, 18),
            (18, 48),
            (0, 5),
            (5, 0),
            (1_000_000, 48),
            (-12, 8),
            (12, -8),
            (i32::MAX, 1),
            (i32::MIN, 2),
            (7, 13),
            (270, 192),
            (1, i32::MAX),
        ] {
            cases.push(("gcd", a, b, &gcd, false));
        }
        for n in [
            1, 2, 3, 6, 7, 9, 27, 55, 97, 171, 703, 871, 6171, 2000, 50000, 100000,
        ] {
            cases.push(("collatz", n, 0, &collatz, false));
        }
        // Random gcd pairs (Euclid terminates for every int pair).
        let mut s: u64 = 0xABCD_1234_5678_9EF1;
        let mut next = || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            s
        };
        for _ in 0..120 {
            cases.push(("gcd", next() as i32, next() as i32, &gcd, false));
        }

        let lines: Vec<String> = cases
            .iter()
            .map(|(m, a, b, _, _)| format!("{m} {a} {b}"))
            .collect();
        let Some(oracle_results) = run_loops_oracle(&dir, &lines) else {
            eprintln!("skipping differential_loops: could not run the oracle");
            return;
        };
        assert_eq!(oracle_results.len(), cases.len());

        for ((m, a, b, built, is_long), &expected) in cases.iter().zip(&oracle_results) {
            let args = if *m == "gcd" {
                vec![Val128::from_i32(*a), Val128::from_i32(*b)]
            } else {
                vec![Val128::from_i32(*a)]
            };
            let r = match execute(built, &args) {
                Ok(v) => v,
                Err(e) => panic!("{m}({a}, {b}) execute failed: {e:?}"),
            };
            let got = if *is_long {
                r.as_i64()
            } else {
                r.as_i32() as i64
            };
            assert_eq!(got, expected, "{m}({a}, {b}) diverged from Corretto 21");
        }
        eprintln!(
            "loops differential OK: {} cases match Corretto 21",
            cases.len()
        );
    }

    fn run_named_oracle(dir: &std::path::Path, class: &str, lines: &[String]) -> Option<Vec<i64>> {
        use std::io::Write;
        let stdin_data = lines.join("\n");
        let mut child = std::process::Command::new("java")
            .arg("-cp")
            .arg(dir)
            .arg(class)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .ok()?;
        child.stdin.take()?.write_all(stdin_data.as_bytes()).ok()?;
        let out = child.wait_with_output().ok()?;
        if !out.status.success() {
            eprintln!(
                "calls oracle failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return None;
        }
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().parse::<i64>().ok())
            .collect()
    }

    /// Increment-2b conformance gate: intra-class `invokestatic`, recursion, and mutual recursion.
    #[test]
    fn differential_calls_vs_corretto_21() {
        if !tool_ok("javac") || !tool_ok("java") {
            eprintln!("skipping differential_calls: JDK unavailable");
            return;
        }
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java/Calls.java");
        let dir = std::env::temp_dir().join(format!("rjava-calls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let oracle = r#"
import java.io.*;
public final class CallsOracle {
    public static void main(String[] x) throws Exception {
        var br = new BufferedReader(new InputStreamReader(System.in));
        var sb = new StringBuilder();
        String line;
        while ((line = br.readLine()) != null) {
            if (line.isEmpty()) continue;
            String[] p = line.split("\\s+");
            int a = Integer.parseInt(p[1]);
            int b = p.length > 2 ? Integer.parseInt(p[2]) : 0;
            int c = p.length > 3 ? Integer.parseInt(p[3]) : 0;
            long r;
            switch (p[0]) {
                case "fib": r = Calls.fib(a); break;
                case "fact": r = Calls.fact(a); break;
                case "isEven": r = Calls.isEven(a); break;
                case "isOdd": r = Calls.isOdd(a); break;
                case "add": r = Calls.add(a, b); break;
                case "addAll": r = Calls.addAll(a, b, c); break;
                default: r = 0;
            }
            sb.append(r).append('\n');
        }
        System.out.print(sb);
    }
}
"#;
        std::fs::write(dir.join("CallsOracle.java"), oracle).unwrap();
        let compiled = std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&src)
            .arg(dir.join("CallsOracle.java"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !compiled {
            eprintln!("skipping differential_calls: javac cannot target --release 21");
            return;
        }

        let bytes = std::fs::read(dir.join("Calls.class")).unwrap();
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let program = Program::from_class(&cf);

        // (method, descriptor, a, b, c, result_is_long)
        let mut cases: Vec<(&str, &str, i32, i32, i32, bool)> = Vec::new();
        for n in 0..=25 {
            cases.push(("fib", "(I)I", n, 0, 0, false));
        }
        for n in 0..=20 {
            cases.push(("fact", "(I)J", n, 0, 0, true));
        }
        for n in [0, 1, 2, 3, 10, 50, 99, 100, 200] {
            // Depth ≈ n, kept inside DEFAULT_MAX_CALL_DEPTH; deeper mutual recursion is covered by
            // `deep_recursion_needs_a_raised_limit_and_a_bigger_stack`.
            cases.push(("isEven", "(I)I", n, 0, 0, false));
            cases.push(("isOdd", "(I)I", n, 0, 0, false));
        }
        for (a, b) in [(3, 4), (-5, 10), (i32::MAX, 1), (i32::MIN, -1), (0, 0)] {
            cases.push(("add", "(II)I", a, b, 0, false));
        }
        for (a, b, c) in [(1, 2, 3), (-1, -2, -3), (i32::MAX, 1, 1), (100, 200, 300)] {
            cases.push(("addAll", "(III)I", a, b, c, false));
        }

        let lines: Vec<String> = cases
            .iter()
            .map(|(m, _, a, b, c, _)| format!("{m} {a} {b} {c}"))
            .collect();
        let Some(oracle_results) = run_named_oracle(&dir, "CallsOracle", &lines) else {
            eprintln!("skipping differential_calls: could not run the oracle");
            return;
        };
        assert_eq!(oracle_results.len(), cases.len());

        for ((m, desc, a, b, c, is_long), &expected) in cases.iter().zip(&oracle_results) {
            let args: Vec<Val128> = match *desc {
                "(II)I" => vec![Val128::from_i32(*a), Val128::from_i32(*b)],
                "(III)I" => vec![
                    Val128::from_i32(*a),
                    Val128::from_i32(*b),
                    Val128::from_i32(*c),
                ],
                _ => vec![Val128::from_i32(*a)],
            };
            let r = program
                .call_named(m, desc, &args)
                .unwrap_or_else(|e| panic!("{m}({a},{b},{c}) failed: {e:?}"))
                .expect("non-void return");
            let got = if *is_long {
                r.as_i64()
            } else {
                r.as_i32() as i64
            };
            assert_eq!(
                got, expected,
                "{m}({a}, {b}, {c}) diverged from Corretto 21"
            );
        }
        eprintln!(
            "calls differential OK: {} cases match Corretto 21",
            cases.len()
        );
    }

    /// Increment-2b StackOverflow seam: unbounded recursion must raise `StackOverflow`, not crash.
    /// Runs on a large stack so the host can hold MAX_CALL_DEPTH frames.
    #[test]
    fn stack_overflow_on_infinite_recursion() {
        if !tool_ok("javac") {
            eprintln!("skipping stack_overflow: javac unavailable");
            return;
        }
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java/Calls.java");
        let dir = std::env::temp_dir().join(format!("rjava-soe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let compiled = std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !compiled {
            eprintln!("skipping stack_overflow: javac cannot target --release 21");
            return;
        }
        let bytes = std::fs::read(dir.join("Calls.class")).unwrap();
        // Regression for a review finding: on a DEFAULT-sized host stack, unbounded guest recursion
        // must return StackOverflow rather than abort the process with a host stack overflow. The
        // thread is deliberately given the default stack size — no 64 MiB crutch.
        let result = std::thread::Builder::new()
            .spawn(move || {
                let cf = rjava_classfile::parse(&bytes).unwrap();
                let program = Program::from_class(&cf); // DEFAULT_MAX_CALL_DEPTH
                program.call_named("deep", "(I)I", &[Val128::from_i32(0)])
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(
            result,
            Err(ExecError::StackOverflow),
            "unbounded recursion must raise StackOverflow, got {result:?}"
        );
        eprintln!("StackOverflow seam OK: deep(0) -> {result:?}");
    }

    /// The documented way to run guest code that recurses deeper than the safe-by-default limit:
    /// give the host thread a bigger stack and raise the limit together. (Until the interpreter
    /// uses an explicit frame stack, guest depth is bounded by host stack space.)
    #[test]
    fn deep_recursion_needs_a_raised_limit_and_a_bigger_stack() {
        if !tool_ok("javac") {
            eprintln!("skipping deep_recursion: javac unavailable");
            return;
        }
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java/Calls.java");
        let dir = std::env::temp_dir().join(format!("rjava-deep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        if !std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            eprintln!("skipping deep_recursion: javac cannot target --release 21");
            return;
        }
        let bytes = std::fs::read(dir.join("Calls.class")).unwrap();
        let got = std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(move || {
                let cf = rjava_classfile::parse(&bytes).unwrap();
                let program = Program::from_class(&cf).with_max_call_depth(4000);
                // isEven(3000) recurses ~3000 frames — far past DEFAULT_MAX_CALL_DEPTH.
                program
                    .call_named("isEven", "(I)I", &[Val128::from_i32(3000)])
                    .unwrap()
                    .unwrap()
                    .as_i32()
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(got, 1, "isEven(3000) is true");
        eprintln!("raised-limit deep recursion OK: isEven(3000) -> {got}");
    }

    /// Stack-φ conformance: native ternary `?:` (including nested) is lowered via φ over
    /// operand-stack slots; every result must match Corretto 21.
    #[test]
    fn differential_ternary_vs_corretto_21() {
        if !tool_ok("javac") || !tool_ok("java") {
            eprintln!("skipping differential_ternary: JDK unavailable");
            return;
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/java/Ternary.java");
        let dir = std::env::temp_dir().join(format!("rjava-tern-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let oracle = r#"
import java.io.*;
public final class TernaryOracle {
    public static void main(String[] x) throws Exception {
        var br = new BufferedReader(new InputStreamReader(System.in));
        var sb = new StringBuilder();
        String line;
        while ((line = br.readLine()) != null) {
            if (line.isEmpty()) continue;
            String[] p = line.split("\\s+");
            int a = Integer.parseInt(p[1]);
            int b = p.length > 2 ? Integer.parseInt(p[2]) : 0;
            int c = p.length > 3 ? Integer.parseInt(p[3]) : 0;
            int r;
            switch (p[0]) {
                case "max": r = Ternary.max(a, b); break;
                case "min": r = Ternary.min(a, b); break;
                case "abs": r = Ternary.abs(a); break;
                case "sign": r = Ternary.sign(a); break;
                case "clamp": r = Ternary.clamp(a, b, c); break;
                case "med3": r = Ternary.med3(a, b, c); break;
                case "select": r = Ternary.select(a, b, c); break;
                default: r = 0;
            }
            sb.append(r).append('\n');
        }
        System.out.print(sb);
    }
}
"#;
        std::fs::write(dir.join("TernaryOracle.java"), oracle).unwrap();
        let compiled = std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&src)
            .arg(dir.join("TernaryOracle.java"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !compiled {
            eprintln!("skipping differential_ternary: javac cannot target --release 21");
            return;
        }
        let bytes = std::fs::read(dir.join("Ternary.class")).unwrap();
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let program = Program::from_class(&cf);

        let vals = [
            0,
            1,
            -1,
            2,
            -2,
            5,
            -5,
            100,
            -100,
            42,
            -42,
            i32::MIN,
            i32::MAX,
            i32::MIN + 1,
            i32::MAX - 1,
        ];
        let mut cases: Vec<(&str, &str, i32, i32, i32)> = Vec::new();
        for &n in &vals {
            cases.push(("abs", "(I)I", n, 0, 0));
            cases.push(("sign", "(I)I", n, 0, 0));
        }
        for &a in &vals {
            for &b in &[0, 1, -1, 100, -100, i32::MIN, i32::MAX] {
                cases.push(("max", "(II)I", a, b, 0));
                cases.push(("min", "(II)I", a, b, 0));
            }
        }
        let mut s: u64 = 0x1357_9BDF_2468_ACE0;
        let mut next = || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            s
        };
        for _ in 0..80 {
            let (a, b, c) = (next() as i32, next() as i32, next() as i32);
            cases.push(("clamp", "(III)I", a, b, c));
            cases.push(("med3", "(III)I", a, b, c));
            cases.push(("select", "(III)I", a, b, c));
        }

        let lines: Vec<String> = cases
            .iter()
            .map(|(m, _, a, b, c)| format!("{m} {a} {b} {c}"))
            .collect();
        let Some(oracle_results) = run_named_oracle(&dir, "TernaryOracle", &lines) else {
            eprintln!("skipping differential_ternary: could not run the oracle");
            return;
        };
        assert_eq!(oracle_results.len(), cases.len());

        for ((m, desc, a, b, c), &expected) in cases.iter().zip(&oracle_results) {
            let args: Vec<Val128> = match *desc {
                "(I)I" => vec![Val128::from_i32(*a)],
                "(II)I" => vec![Val128::from_i32(*a), Val128::from_i32(*b)],
                _ => vec![
                    Val128::from_i32(*a),
                    Val128::from_i32(*b),
                    Val128::from_i32(*c),
                ],
            };
            let r = program
                .call_named(m, desc, &args)
                .unwrap_or_else(|e| panic!("{m}({a},{b},{c}) failed: {e:?}"))
                .expect("non-void return")
                .as_i32() as i64;
            assert_eq!(r, expected, "{m}({a}, {b}, {c}) diverged from Corretto 21");
        }
        eprintln!(
            "ternary differential OK: {} cases match Corretto 21",
            cases.len()
        );
    }

    /// Interaction (联动) gate: loops + recursive/iterative calls + ternary + int/long arithmetic
    /// combined in single methods, all matching Corretto 21.
    #[test]
    fn differential_mixed_vs_corretto_21() {
        if !tool_ok("javac") || !tool_ok("java") {
            eprintln!("skipping differential_mixed: JDK unavailable");
            return;
        }
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java/Mixed.java");
        let dir = std::env::temp_dir().join(format!("rjava-mixed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let oracle = r#"
import java.io.*;
public final class MixedOracle {
    public static void main(String[] x) throws Exception {
        var br = new BufferedReader(new InputStreamReader(System.in));
        var sb = new StringBuilder();
        String line;
        while ((line = br.readLine()) != null) {
            if (line.isEmpty()) continue;
            String[] p = line.split("\\s+");
            int a = Integer.parseInt(p[1]);
            int b = p.length > 2 ? Integer.parseInt(p[2]) : 0;
            long r;
            switch (p[0]) {
                case "fib": r = Mixed.fib(a); break;
                case "gcd": r = Mixed.gcd(a, b); break;
                case "sumFibSigned": r = Mixed.sumFibSigned(a); break;
                case "totient": r = Mixed.totient(a); break;
                case "collatzLen": r = Mixed.collatzLen(a); break;
                default: r = 0;
            }
            sb.append(r).append('\n');
        }
        System.out.print(sb);
    }
}
"#;
        std::fs::write(dir.join("MixedOracle.java"), oracle).unwrap();
        let compiled = std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&src)
            .arg(dir.join("MixedOracle.java"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !compiled {
            eprintln!("skipping differential_mixed: javac cannot target --release 21");
            return;
        }
        let bytes = std::fs::read(dir.join("Mixed.class")).unwrap();
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let program = Program::from_class(&cf);

        // (method, descriptor, a, b, result_is_long)
        let mut cases: Vec<(&str, &str, i32, i32, bool)> = Vec::new();
        for n in 0..=25 {
            cases.push(("fib", "(I)I", n, 0, false));
        }
        for (a, b) in [
            (48, 18),
            (0, 7),
            (7, 0),
            (270, 192),
            (i32::MAX, 3),
            (-12, 8),
        ] {
            cases.push(("gcd", "(II)I", a, b, false));
        }
        for n in 0..=22 {
            cases.push(("sumFibSigned", "(I)J", n, 0, true));
        }
        for n in [1, 2, 3, 6, 10, 12, 36, 97, 100, 200, 360] {
            cases.push(("totient", "(I)I", n, 0, false));
        }
        for n in [1, 2, 3, 6, 7, 27, 97, 255, 511, 1000] {
            cases.push(("collatzLen", "(I)I", n, 0, false));
        }

        let lines: Vec<String> = cases
            .iter()
            .map(|(m, _, a, b, _)| format!("{m} {a} {b}"))
            .collect();
        let Some(oracle_results) = run_named_oracle(&dir, "MixedOracle", &lines) else {
            eprintln!("skipping differential_mixed: could not run the oracle");
            return;
        };
        assert_eq!(oracle_results.len(), cases.len());

        for ((m, desc, a, b, is_long), &expected) in cases.iter().zip(&oracle_results) {
            let args: Vec<Val128> = if *desc == "(II)I" {
                vec![Val128::from_i32(*a), Val128::from_i32(*b)]
            } else {
                vec![Val128::from_i32(*a)]
            };
            let r = program
                .call_named(m, desc, &args)
                .unwrap_or_else(|e| panic!("{m}({a},{b}) failed: {e:?}"))
                .expect("non-void return");
            let got = if *is_long {
                r.as_i64()
            } else {
                r.as_i32() as i64
            };
            assert_eq!(got, expected, "{m}({a}, {b}) diverged from Corretto 21");
        }
        eprintln!(
            "mixed (interaction) differential OK: {} cases match Corretto 21",
            cases.len()
        );
    }

    /// Increment-3 conformance gate: `new`/getfield/putfield/invokespecial on local (S1) objects,
    /// including objects mutated in a loop; every result must match Corretto 21.
    #[test]
    fn differential_objects_vs_corretto_21() {
        if !tool_ok("javac") || !tool_ok("java") {
            eprintln!("skipping differential_objects: JDK unavailable");
            return;
        }
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java/Point.java");
        let dir = std::env::temp_dir().join(format!("rjava-obj-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let oracle = r#"
import java.io.*;
public final class PointOracle {
    public static void main(String[] x) throws Exception {
        var br = new BufferedReader(new InputStreamReader(System.in));
        var sb = new StringBuilder();
        String line;
        while ((line = br.readLine()) != null) {
            if (line.isEmpty()) continue;
            String[] p = line.split("\\s+");
            int a = p.length > 1 ? Integer.parseInt(p[1]) : 0;
            int b = p.length > 2 ? Integer.parseInt(p[2]) : 0;
            int c = p.length > 3 ? Integer.parseInt(p[3]) : 0;
            int d = p.length > 4 ? Integer.parseInt(p[4]) : 0;
            int r;
            switch (p[0]) {
                case "normSq": r = Point.normSq(a, b); break;
                case "manhattan": r = Point.manhattan(a, b, c, d); break;
                case "defaults": r = Point.defaults(); break;
                case "accumulate": r = Point.accumulate(a); break;
                default: r = 0;
            }
            sb.append(r).append('\n');
        }
        System.out.print(sb);
    }
}
"#;
        std::fs::write(dir.join("PointOracle.java"), oracle).unwrap();
        let compiled = std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&src)
            .arg(dir.join("PointOracle.java"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !compiled {
            eprintln!("skipping differential_objects: javac cannot target --release 21");
            return;
        }
        let bytes = std::fs::read(dir.join("Point.class")).unwrap();
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let program = Program::from_class(&cf);

        // (method, descriptor, a, b, c, d)
        let mut cases: Vec<(&str, &str, i32, i32, i32, i32)> = Vec::new();
        cases.push(("defaults", "()I", 0, 0, 0, 0));
        for (a, b) in [
            (3, 4),
            (0, 0),
            (-5, 12),
            (i32::MAX, 1),
            (46340, 46340),
            (-7, -8),
        ] {
            cases.push(("normSq", "(II)I", a, b, 0, 0));
        }
        for (a, b, c, d) in [
            (0, 0, 3, 4),
            (1, 1, -1, -1),
            (10, 20, 30, 40),
            (i32::MIN, 0, 0, 0),
            (-5, 5, 5, -5),
        ] {
            cases.push(("manhattan", "(IIII)I", a, b, c, d));
        }
        for n in [0, 1, 2, 5, 10, 100, 1000] {
            cases.push(("accumulate", "(I)I", n, 0, 0, 0));
        }

        let lines: Vec<String> = cases
            .iter()
            .map(|(m, _, a, b, c, d)| format!("{m} {a} {b} {c} {d}"))
            .collect();
        let Some(oracle_results) = run_named_oracle(&dir, "PointOracle", &lines) else {
            eprintln!("skipping differential_objects: could not run the oracle");
            return;
        };
        assert_eq!(oracle_results.len(), cases.len());

        for ((m, desc, a, b, c, d), &expected) in cases.iter().zip(&oracle_results) {
            let args: Vec<Val128> = match *desc {
                "()I" => vec![],
                "(I)I" => vec![Val128::from_i32(*a)],
                "(II)I" => vec![Val128::from_i32(*a), Val128::from_i32(*b)],
                _ => vec![
                    Val128::from_i32(*a),
                    Val128::from_i32(*b),
                    Val128::from_i32(*c),
                    Val128::from_i32(*d),
                ],
            };
            let r = program
                .call_named(m, desc, &args)
                .unwrap_or_else(|e| panic!("{m}({a},{b},{c},{d}) failed: {e:?}"))
                .expect("non-void return")
                .as_i32() as i64;
            assert_eq!(
                r, expected,
                "{m}({a}, {b}, {c}, {d}) diverged from Corretto 21"
            );
        }
        eprintln!(
            "objects differential OK: {} cases match Corretto 21",
            cases.len()
        );
    }

    /// Increment-5 conformance gate: escape analysis (S1 vs S2) and the reference-count fast path
    /// must not change observable results — every case still matches Corretto 21.
    #[test]
    fn differential_escape_vs_corretto_21() {
        if !tool_ok("javac") || !tool_ok("java") {
            eprintln!("skipping differential_escape: JDK unavailable");
            return;
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/java/Escape.java");
        let dir = std::env::temp_dir().join(format!("rjava-esc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let oracle = r#"
import java.io.*;
public final class EscapeOracle {
    public static void main(String[] x) throws Exception {
        var br = new BufferedReader(new InputStreamReader(System.in));
        var sb = new StringBuilder();
        String line;
        while ((line = br.readLine()) != null) {
            if (line.isEmpty()) continue;
            String[] p = line.split("\\s+");
            int a = Integer.parseInt(p[1]);
            int b = p.length > 2 ? Integer.parseInt(p[2]) : 0;
            int r;
            switch (p[0]) {
                case "local": r = Escape.local(a); break;
                case "useReturned": r = Escape.useReturned(a); break;
                case "stored": r = Escape.stored(a, b); break;
                case "chain": r = Escape.chain(a); break;
                case "churn": r = Escape.churn(a); break;
                default: r = 0;
            }
            sb.append(r).append('\n');
        }
        System.out.print(sb);
    }
}
"#;
        std::fs::write(dir.join("EscapeOracle.java"), oracle).unwrap();
        if !std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&src)
            .arg(dir.join("EscapeOracle.java"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            eprintln!("skipping differential_escape: javac cannot target --release 21");
            return;
        }
        let bytes = std::fs::read(dir.join("Escape.class")).unwrap();
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let program = Program::from_class(&cf);

        let mut cases: Vec<(&str, &str, i32, i32)> = Vec::new();
        for n in [0, 1, 2, 7, -3, 100, i32::MAX, i32::MIN] {
            cases.push(("local", "(I)I", n, 0));
            cases.push(("useReturned", "(I)I", n, 0));
        }
        for (a, b) in [(1, 2), (0, 0), (-5, 9), (i32::MAX, 1), (100, 200)] {
            cases.push(("stored", "(II)I", a, b));
        }
        for n in [0, 1, 2, 5, 10, 50, 200] {
            cases.push(("chain", "(I)I", n, 0));
            cases.push(("churn", "(I)I", n, 0));
        }

        let lines: Vec<String> = cases
            .iter()
            .map(|(m, _, a, b)| format!("{m} {a} {b}"))
            .collect();
        let Some(expected) = run_named_oracle(&dir, "EscapeOracle", &lines) else {
            eprintln!("skipping differential_escape: could not run the oracle");
            return;
        };
        assert_eq!(expected.len(), cases.len());

        for ((m, desc, a, b), &want) in cases.iter().zip(&expected) {
            let args: Vec<Val128> = if *desc == "(II)I" {
                vec![Val128::from_i32(*a), Val128::from_i32(*b)]
            } else {
                vec![Val128::from_i32(*a)]
            };
            let got = program
                .call_named(m, desc, &args)
                .unwrap_or_else(|e| panic!("{m}({a},{b}) failed: {e:?}"))
                .unwrap_or_else(|| panic!("{m}({a},{b}) returned void"))
                .as_i32() as i64;
            assert_eq!(got, want, "{m}({a}, {b}) diverged from Corretto 21");
        }
        eprintln!(
            "escape/rc differential OK: {} cases match Corretto 21",
            cases.len()
        );
    }

    /// The reference-count fast path must actually reclaim: after a call tree that allocates
    /// hundreds of objects — local (S1, RAII) and escaping (S2, counted) alike — the heap must be
    /// empty again, and the count must have driven that (§5.1, §5.2, §6.2).
    #[test]
    fn objects_are_reclaimed_when_unreferenced() {
        if !tool_ok("javac") {
            eprintln!("skipping reclamation test: javac unavailable");
            return;
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/java/Escape.java");
        let dir = std::env::temp_dir().join(format!("rjava-rc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        if !std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            eprintln!("skipping reclamation test: javac cannot target --release 21");
            return;
        }
        let bytes = std::fs::read(dir.join("Escape.class")).unwrap();
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let program = Program::from_class(&cf);

        for (m, desc, arg) in [
            ("churn", "(I)I", 300),     // S1 objects, reclaimed by RAII at scope exit
            ("chain", "(I)I", 200),     // an S2 chain, reclaimed by cascading rc release
            ("useReturned", "(I)I", 5), // an S2 object handed across a frame boundary
            ("local", "(I)I", 3),
        ] {
            let mut heap = Heap::new();
            let idx = program.method_index(m, desc).expect("method present");
            program
                .call(idx, &[Val128::from_i32(arg)], 0, &mut heap)
                .unwrap_or_else(|e| panic!("{m}({arg}) failed: {e:?}"));
            assert_eq!(
                heap.live(),
                0,
                "{m}({arg}) leaked {} objects — every allocation must be reclaimed once \
                 unreferenced",
                heap.live()
            );
        }
        eprintln!("reference-count reclamation OK: no leaks across churn/chain/useReturned/local");
    }
}
