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

use rjava_core::Terminator;
use rjava_core::{Env, IntCond, LogicalFrame, Op, SlotId, Tag, Val128};
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
}

/// Execute a built method against `args` (in declaration order), returning the result value.
pub fn execute(built: &BuiltMethod, args: &[Val128]) -> Result<Val128, ExecError> {
    if args.len() != built.arg_vals.len() {
        return Err(ExecError::BadArgs);
    }
    let mut env = Env::new(built.n_slots.max(1), LogicalFrame(0));
    for (slot, &value) in built.arg_vals.iter().zip(args) {
        env.write_slot(SlotId(slot.offset), value);
    }

    let mut cur = built.method.entry;
    let mut steps: u64 = 0;
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

        for node in &block.nodes {
            let mut inputs: smallvec::SmallVec<[Val128; 3]> = smallvec::SmallVec::new();
            for v in &node.ins {
                inputs.push(env.read_slot(SlotId(v.offset)));
            }
            let result = eval(node.op, node.ty, &inputs)?;
            env.write_slot(SlotId(node.id.offset), result);
        }

        match &block.term {
            Terminator::Return(Some(v)) => return Ok(env.read_slot(SlotId(v.offset))),
            Terminator::Return(None) => return Ok(Val128::null()),
            Terminator::Goto(b) => cur = *b,
            Terminator::CondBranch {
                cond,
                taken,
                not_taken,
            } => {
                let c = env.read_slot(SlotId(cond.offset)).as_i32();
                cur = if c != 0 { *taken } else { *not_taken };
            }
            Terminator::Throw(_) => return Err(ExecError::Thrown),
        }
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
        let built = build(&vm, &cf.constant_pool).unwrap();

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
}
