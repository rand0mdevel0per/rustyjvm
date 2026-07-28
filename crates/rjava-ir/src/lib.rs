//! rjava-ir — L1 SSA construction (RJVM-SPEC-001 §9.2). Converts a verified stack-based method
//! into a register-based SSA data-flow tree: basic blocks, SSA `Node`s whose `ins` edges ARE the
//! dependency set that drives out-of-order issue (§10), and `Terminator`s. Increment 1 handles the
//! vertical-slice opcode set with straight-line-dominated locals (no φ needed yet); general φ
//! placement for loop/merge variables arrives with loops in increment 2.
//!
//! The builder trusts its input has passed verification (§7.5) but still returns errors rather than
//! panicking on anything unexpected (defence in depth).

use std::collections::{BTreeSet, HashMap};

use rjava_classfile::{Constant, ConstantPool};
use rjava_core::{
    Block, BlockId, Effect, IntCond, Method, Node, Op, Tag, Terminator, Val128, ValId,
};
use rjava_verify::{Insn, VerifiedMethod};
use smallvec::{smallvec, SmallVec};

/// Failure while building L1 IR. For verified input these are unreachable; they exist so the
/// builder never panics.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IrError {
    #[error("opcode {op:#04x} at pc {pc} has no IR lowering (increment-1 subset)")]
    Unsupported { op: u8, pc: u32 },
    #[error("operand stack underflow at pc {0}")]
    StackUnderflow(u32),
    #[error("read of undefined local {index} at pc {pc}")]
    BadLocalRead { pc: u32, index: usize },
    #[error("bad constant reference at pc {0}")]
    BadConstant(u32),
    #[error("control-flow target has no block at pc {0}")]
    BadBlock(u32),
    #[error("method exceeds the per-scope SSA value limit")]
    TooManyValues,
}

/// An L1 method plus what the interpreter needs to run it.
pub struct BuiltMethod {
    pub method: Method,
    /// SSA values bound to the incoming arguments, in order (`this` first for instance methods).
    pub arg_vals: Vec<ValId>,
    /// Number of Env slots required (every `ValId.offset` is `< n_slots`).
    pub n_slots: usize,
}

fn fresh(next: &mut u16) -> Result<ValId, IrError> {
    let id = ValId {
        scope_level: 0,
        offset: *next,
    };
    *next = next.checked_add(1).ok_or(IrError::TooManyValues)?;
    Ok(id)
}

fn push_val(
    stack: &mut Vec<ValId>,
    nodes: &mut Vec<Node>,
    next: &mut u16,
    op: Op,
    ins: SmallVec<[ValId; 3]>,
    ty: Tag,
    effect: Effect,
) -> Result<(), IrError> {
    let id = fresh(next)?;
    nodes.push(Node {
        id,
        op,
        ins,
        ty,
        effect,
    });
    stack.push(id);
    Ok(())
}

fn binop(
    stack: &mut Vec<ValId>,
    nodes: &mut Vec<Node>,
    next: &mut u16,
    pc: u32,
    op: Op,
    ty: Tag,
    effect: Effect,
) -> Result<(), IrError> {
    let b = stack.pop().ok_or(IrError::StackUnderflow(pc))?;
    let a = stack.pop().ok_or(IrError::StackUnderflow(pc))?;
    push_val(stack, nodes, next, op, smallvec![a, b], ty, effect)
}

fn unop(
    stack: &mut Vec<ValId>,
    nodes: &mut Vec<Node>,
    next: &mut u16,
    pc: u32,
    op: Op,
    ty: Tag,
) -> Result<(), IrError> {
    let a = stack.pop().ok_or(IrError::StackUnderflow(pc))?;
    push_val(stack, nodes, next, op, smallvec![a], ty, Effect::Pure)
}

fn konst(
    stack: &mut Vec<ValId>,
    nodes: &mut Vec<Node>,
    next: &mut u16,
    val: Val128,
) -> Result<(), IrError> {
    let ty = val.tag();
    push_val(
        stack,
        nodes,
        next,
        Op::Const(val),
        smallvec![],
        ty,
        Effect::Pure,
    )
}

fn cp_const(cp: &ConstantPool, index: i64, pc: u32) -> Result<Val128, IrError> {
    let idx = u16::try_from(index).map_err(|_| IrError::BadConstant(pc))?;
    Ok(match cp.get(idx) {
        Some(Constant::Integer(v)) => Val128::from_i32(*v),
        Some(Constant::Float(v)) => Val128::from_f32(*v),
        Some(Constant::Long(v)) => Val128::from_i64(*v),
        Some(Constant::Double(v)) => Val128::from_f64(*v),
        _ => return Err(IrError::BadConstant(pc)),
    })
}

fn int_cond(op: u8) -> IntCond {
    match op {
        0x99 => IntCond::Eq,
        0x9a => IntCond::Ne,
        0x9b => IntCond::Lt,
        0x9c => IntCond::Ge,
        0x9d => IntCond::Gt,
        _ => IntCond::Le, // 0x9e
    }
}

/// Compute the basic-block leaders: entry, every branch target, and the instruction after every
/// branch or `ireturn`.
fn leaders(insns: &[Insn], valid: &HashMap<u32, usize>) -> Vec<u32> {
    let mut set = BTreeSet::new();
    if let Some(first) = insns.first() {
        set.insert(first.pc);
    }
    for ins in insns {
        if let Some(t) = ins.branch_target() {
            set.insert(t);
        }
        if ins.branch_target().is_some() || ins.is_unconditional_end() {
            let next = ins.pc + ins.len as u32;
            if valid.contains_key(&next) {
                set.insert(next);
            }
        }
    }
    set.into_iter().collect()
}

/// Build the L1 SSA form of a verified method.
pub fn build(vm: &VerifiedMethod, cp: &ConstantPool) -> Result<BuiltMethod, IrError> {
    let valid: HashMap<u32, usize> = vm
        .insns
        .iter()
        .enumerate()
        .map(|(i, ins)| (ins.pc, i))
        .collect();
    let leader_pcs = leaders(&vm.insns, &valid);
    let block_of: HashMap<u32, BlockId> = leader_pcs
        .iter()
        .enumerate()
        .map(|(i, &pc)| (pc, BlockId(i as u32)))
        .collect();

    // Partition instructions into blocks (they are already in pc order).
    let mut block_insns: Vec<Vec<Insn>> = vec![Vec::new(); leader_pcs.len()];
    let mut bi = 0;
    for ins in &vm.insns {
        while bi + 1 < leader_pcs.len() && ins.pc >= leader_pcs[bi + 1] {
            bi += 1;
        }
        block_insns[bi].push(*ins);
    }

    // Bind arguments to SSA values and seed the locals map.
    let mut next: u16 = 0;
    let mut locals: Vec<Option<ValId>> = vec![None; vm.max_locals as usize];
    let mut arg_vals = Vec::new();
    {
        let mut slot = 0usize;
        if !vm.is_static {
            let v = fresh(&mut next)?;
            locals[slot] = Some(v);
            arg_vals.push(v);
            slot += 1;
        }
        for &t in &vm.arg_types {
            let v = fresh(&mut next)?;
            *locals.get_mut(slot).ok_or(IrError::BadBlock(0))? = Some(v);
            arg_vals.push(v);
            slot += t.size() as usize;
        }
    }

    let mut blocks: Vec<Block> = Vec::with_capacity(leader_pcs.len());
    for (bidx, block) in block_insns.iter().enumerate() {
        let mut stack: Vec<ValId> = Vec::new(); // verified empty at every block boundary
        let mut nodes: Vec<Node> = Vec::new();
        let mut term: Option<Terminator> = None;

        for ins in block {
            let pc = ins.pc;
            match ins.op {
                0x00 => {}
                // constants
                0x02..=0x08 => konst(
                    &mut stack,
                    &mut nodes,
                    &mut next,
                    Val128::from_i32(ins.op as i32 - 3),
                )?,
                0x09 | 0x0a => konst(
                    &mut stack,
                    &mut nodes,
                    &mut next,
                    Val128::from_i64((ins.op - 0x09) as i64),
                )?,
                0x0b..=0x0d => konst(
                    &mut stack,
                    &mut nodes,
                    &mut next,
                    Val128::from_f32((ins.op - 0x0b) as f32),
                )?,
                0x10 | 0x11 => konst(
                    &mut stack,
                    &mut nodes,
                    &mut next,
                    Val128::from_i32(ins.arg as i32),
                )?,
                0x12..=0x14 => {
                    let v = cp_const(cp, ins.arg, pc)?;
                    konst(&mut stack, &mut nodes, &mut next, v)?;
                }
                // loads
                0x15..=0x17 => load(&mut stack, &locals, ins.arg as usize, pc)?,
                0x1a..=0x1d => load(&mut stack, &locals, (ins.op - 0x1a) as usize, pc)?,
                0x1e..=0x21 => load(&mut stack, &locals, (ins.op - 0x1e) as usize, pc)?,
                0x22..=0x25 => load(&mut stack, &locals, (ins.op - 0x22) as usize, pc)?,
                // stores
                0x36..=0x38 => store(&mut stack, &mut locals, ins.arg as usize, pc)?,
                0x3b..=0x3e => store(&mut stack, &mut locals, (ins.op - 0x3b) as usize, pc)?,
                0x3f..=0x42 => store(&mut stack, &mut locals, (ins.op - 0x3f) as usize, pc)?,
                0x43..=0x46 => store(&mut stack, &mut locals, (ins.op - 0x43) as usize, pc)?,
                // integer arithmetic
                0x60 => binop(
                    &mut stack,
                    &mut nodes,
                    &mut next,
                    pc,
                    Op::Add,
                    Tag::I32,
                    Effect::Pure,
                )?,
                0x64 => binop(
                    &mut stack,
                    &mut nodes,
                    &mut next,
                    pc,
                    Op::Sub,
                    Tag::I32,
                    Effect::Pure,
                )?,
                0x68 => binop(
                    &mut stack,
                    &mut nodes,
                    &mut next,
                    pc,
                    Op::Mul,
                    Tag::I32,
                    Effect::Pure,
                )?,
                0x6c => binop(
                    &mut stack,
                    &mut nodes,
                    &mut next,
                    pc,
                    Op::Div,
                    Tag::I32,
                    Effect::MayThrow { caught: false },
                )?,
                0x70 => binop(
                    &mut stack,
                    &mut nodes,
                    &mut next,
                    pc,
                    Op::Rem,
                    Tag::I32,
                    Effect::MayThrow { caught: false },
                )?,
                0x74 => unop(&mut stack, &mut nodes, &mut next, pc, Op::Neg, Tag::I32)?,
                // long arithmetic
                0x61 => binop(
                    &mut stack,
                    &mut nodes,
                    &mut next,
                    pc,
                    Op::Add,
                    Tag::I64,
                    Effect::Pure,
                )?,
                0x65 => binop(
                    &mut stack,
                    &mut nodes,
                    &mut next,
                    pc,
                    Op::Sub,
                    Tag::I64,
                    Effect::Pure,
                )?,
                0x69 => binop(
                    &mut stack,
                    &mut nodes,
                    &mut next,
                    pc,
                    Op::Mul,
                    Tag::I64,
                    Effect::Pure,
                )?,
                // float arithmetic
                0x62 => binop(
                    &mut stack,
                    &mut nodes,
                    &mut next,
                    pc,
                    Op::Add,
                    Tag::F32,
                    Effect::Pure,
                )?,
                0x66 => binop(
                    &mut stack,
                    &mut nodes,
                    &mut next,
                    pc,
                    Op::Sub,
                    Tag::F32,
                    Effect::Pure,
                )?,
                0x6a => binop(
                    &mut stack,
                    &mut nodes,
                    &mut next,
                    pc,
                    Op::Mul,
                    Tag::F32,
                    Effect::Pure,
                )?,
                // conversions (result type from the opcode)
                0x85 => unop(&mut stack, &mut nodes, &mut next, pc, Op::Convert, Tag::I64)?, // i2l
                0x86 => unop(&mut stack, &mut nodes, &mut next, pc, Op::Convert, Tag::F32)?, // i2f
                0x88 => unop(&mut stack, &mut nodes, &mut next, pc, Op::Convert, Tag::I32)?, // l2i
                0x89 => unop(&mut stack, &mut nodes, &mut next, pc, Op::Convert, Tag::F32)?, // l2f
                0x8b => unop(&mut stack, &mut nodes, &mut next, pc, Op::Convert, Tag::I32)?, // f2i
                0x8c => unop(&mut stack, &mut nodes, &mut next, pc, Op::Convert, Tag::I64)?, // f2l
                // compares (3-way -> int)
                0x94 | 0x95 => binop(
                    &mut stack,
                    &mut nodes,
                    &mut next,
                    pc,
                    Op::Cmp { nan_greater: false },
                    Tag::I32,
                    Effect::Pure,
                )?,
                0x96 => binop(
                    &mut stack,
                    &mut nodes,
                    &mut next,
                    pc,
                    Op::Cmp { nan_greater: true },
                    Tag::I32,
                    Effect::Pure,
                )?,
                // branches
                0x99..=0x9e => {
                    let v = stack.pop().ok_or(IrError::StackUnderflow(pc))?;
                    let cond = fresh(&mut next)?;
                    nodes.push(Node {
                        id: cond,
                        op: Op::TestZero(int_cond(ins.op)),
                        ins: smallvec![v],
                        ty: Tag::I32,
                        effect: Effect::Pure,
                    });
                    let taken = *block_of
                        .get(&ins.branch_target().unwrap())
                        .ok_or(IrError::BadBlock(pc))?;
                    let not_taken = *block_of
                        .get(&(pc + ins.len as u32))
                        .ok_or(IrError::BadBlock(pc))?;
                    term = Some(Terminator::CondBranch {
                        cond,
                        taken,
                        not_taken,
                    });
                }
                0xa7 => {
                    let taken = *block_of
                        .get(&ins.branch_target().unwrap())
                        .ok_or(IrError::BadBlock(pc))?;
                    term = Some(Terminator::Goto(taken));
                }
                0xac => {
                    let v = stack.pop().ok_or(IrError::StackUnderflow(pc))?;
                    term = Some(Terminator::Return(Some(v)));
                }
                _ => return Err(IrError::Unsupported { op: ins.op, pc }),
            }
        }

        let terminator = match term {
            Some(t) => t,
            None => {
                // Fall through to the next block.
                let next_block = bidx + 1;
                if next_block < leader_pcs.len() {
                    Terminator::Goto(BlockId(next_block as u32))
                } else {
                    return Err(IrError::BadBlock(block.last().map_or(0, |i| i.pc)));
                }
            }
        };

        blocks.push(Block {
            id: BlockId(bidx as u32),
            phis: Vec::new(),
            nodes,
            term: terminator,
        });
    }

    Ok(BuiltMethod {
        method: Method {
            blocks,
            entry: BlockId(0),
            max_locals: vm.max_locals,
            exc_table: Vec::new(),
        },
        arg_vals,
        n_slots: next as usize,
    })
}

fn load(
    stack: &mut Vec<ValId>,
    locals: &[Option<ValId>],
    idx: usize,
    pc: u32,
) -> Result<(), IrError> {
    let v = locals
        .get(idx)
        .copied()
        .flatten()
        .ok_or(IrError::BadLocalRead { pc, index: idx })?;
    stack.push(v);
    Ok(())
}

fn store(
    stack: &mut Vec<ValId>,
    locals: &mut [Option<ValId>],
    idx: usize,
    pc: u32,
) -> Result<(), IrError> {
    let v = stack.pop().ok_or(IrError::StackUnderflow(pc))?;
    *locals
        .get_mut(idx)
        .ok_or(IrError::BadLocalRead { pc, index: idx })? = Some(v);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile_slice() -> Option<Vec<u8>> {
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java/Slice.java");
        if !src.exists() {
            return None;
        }
        let out = std::env::temp_dir().join(format!("rjava-ir-{}", std::process::id()));
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
    fn builds_slice_arith_ssa() {
        let Some(bytes) = compile_slice() else {
            eprintln!("skipping builds_slice_arith_ssa: javac unavailable");
            return;
        };
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let arith = cf.method("arith", "(IIJF)I").unwrap();
        let vm = rjava_verify::verify_method(&cf, arith).unwrap();
        let built = build(&vm, &cf.constant_pool).unwrap();

        assert_eq!(built.arg_vals.len(), 4); // a, b, c, d
        assert_eq!(built.method.blocks.len(), 6); // leaders 0,44,52,63,70,76
        assert_eq!(built.method.entry, BlockId(0));
        // Every block ends in a real terminator; the entry block ends in a conditional branch.
        assert!(matches!(
            built.method.blocks[0].term,
            Terminator::CondBranch { .. }
        ));
        // Some block returns a value.
        assert!(built
            .method
            .blocks
            .iter()
            .any(|b| matches!(b.term, Terminator::Return(Some(_)))));
        // SSA offsets are dense and within n_slots.
        for b in &built.method.blocks {
            for n in &b.nodes {
                assert!((n.id.offset as usize) < built.n_slots);
            }
        }
    }
}
