//! rjava-ir — L1 SSA construction (RJVM-SPEC-001 §9.2), now with loop support. SSA is built
//! directly from bytecode using Braun et al.'s algorithm ("Simple and Efficient Construction of
//! SSA Form"): per-block variable definitions, sealed blocks, and incomplete phis correctly place
//! φ nodes at loop headers (back-edges) without a separate dominance pass. Trivial-phi removal is
//! deferred (redundant φ are correct, just non-minimal). The operand stack is assumed empty at
//! block boundaries (true for javac's structured control flow); non-empty-stack merges are added
//! when a fixture needs them.
//!
//! `Node.ins` edges ARE the dependency set that later drives out-of-order/speculative issue (§10);
//! building real φ now — rather than keeping locals mutable — keeps that substrate faithful so the
//! diff/fork machinery (increment 4) is a pure addition.

use std::collections::{BTreeSet, HashMap, HashSet};

use rjava_classfile::{ClassFile, Constant, ConstantPool};
use rjava_core::{
    Block, BlockId, Effect, IntCond, Method, Node, Op, Phi, Tag, Terminator, Val128, ValId,
};
use rjava_verify::{parse_method_descriptor, Insn, VType, VerifiedMethod};
use smallvec::{smallvec, SmallVec};

/// Failure while building L1 IR. For verified input these are unreachable; they exist so the
/// builder never panics.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IrError {
    #[error("opcode {op:#04x} at pc {pc} has no IR lowering (increment-2 subset)")]
    Unsupported { op: u8, pc: u32 },
    #[error("operand stack underflow at pc {0}")]
    StackUnderflow(u32),
    #[error("read of undefined local {index}")]
    BadLocalRead { index: usize },
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

/// Resolve an `invokestatic` methodref to a same-class method index and its descriptor types.
/// Increment 2b supports intra-class static calls only; cross-class dispatch awaits the loader (§8).
fn resolve_invokestatic(
    cf: &ClassFile,
    cpidx: u16,
    pc: u32,
) -> Result<(u16, Vec<VType>, Option<VType>), IrError> {
    let cp = &cf.constant_pool;
    let (class_index, nat_index) = match cp.get(cpidx) {
        Some(Constant::MethodRef {
            class_index,
            name_and_type_index,
        }) => (*class_index, *name_and_type_index),
        _ => return Err(IrError::Unsupported { op: 0xb8, pc }),
    };
    if cp.class_name(class_index) != cf.this_class_name() {
        return Err(IrError::Unsupported { op: 0xb8, pc });
    }
    let (name_index, desc_index) = match cp.get(nat_index) {
        Some(Constant::NameAndType {
            name_index,
            descriptor_index,
        }) => (*name_index, *descriptor_index),
        _ => return Err(IrError::Unsupported { op: 0xb8, pc }),
    };
    let name = cp
        .utf8(name_index)
        .ok_or(IrError::Unsupported { op: 0xb8, pc })?;
    let desc = cp
        .utf8(desc_index)
        .ok_or(IrError::Unsupported { op: 0xb8, pc })?;
    let mindex = cf
        .methods
        .iter()
        .position(|m| m.name(cp) == Some(name) && m.descriptor(cp) == Some(desc))
        .ok_or(IrError::Unsupported { op: 0xb8, pc })?;
    let (arg_ty, ret_ty) =
        parse_method_descriptor(desc).map_err(|_| IrError::Unsupported { op: 0xb8, pc })?;
    Ok((mindex as u16, arg_ty, ret_ty))
}

fn if_cond(op: u8) -> IntCond {
    match op {
        0x99 => IntCond::Eq,
        0x9a => IntCond::Ne,
        0x9b => IntCond::Lt,
        0x9c => IntCond::Ge,
        0x9d => IntCond::Gt,
        _ => IntCond::Le, // 0x9e
    }
}

fn if_icmp_cond(op: u8) -> IntCond {
    match op {
        0x9f => IntCond::Eq,
        0xa0 => IntCond::Ne,
        0xa1 => IntCond::Lt,
        0xa2 => IntCond::Ge,
        0xa3 => IntCond::Gt,
        _ => IntCond::Le, // 0xa4
    }
}

fn is_return(op: u8) -> bool {
    (0xac..=0xb1).contains(&op)
}

/// Basic-block leaders: entry, every branch target, and the instruction after every branch/return.
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

/// A φ under construction: its owning block, the local it merges, its SSA value, and its operands.
struct PhiDef {
    block: BlockId,
    var: u16,
    val: ValId,
    sources: Vec<(BlockId, ValId)>,
}

/// SSA-construction state (Braun et al.).
struct Ssa {
    current_def: HashMap<(u16, BlockId), ValId>,
    phis: Vec<PhiDef>,
    phi_of: HashMap<ValId, usize>,
    incomplete_phis: HashMap<BlockId, Vec<ValId>>,
    sealed: HashSet<BlockId>,
    preds: HashMap<BlockId, Vec<BlockId>>,
    next: u16,
}

impl Ssa {
    fn fresh(&mut self) -> Result<ValId, IrError> {
        let id = ValId {
            scope_level: 0,
            offset: self.next,
        };
        self.next = self.next.checked_add(1).ok_or(IrError::TooManyValues)?;
        Ok(id)
    }

    fn write_variable(&mut self, var: u16, block: BlockId, val: ValId) {
        self.current_def.insert((var, block), val);
    }

    fn read_variable(&mut self, var: u16, block: BlockId) -> Result<ValId, IrError> {
        if let Some(&v) = self.current_def.get(&(var, block)) {
            return Ok(v);
        }
        self.read_variable_recursive(var, block)
    }

    fn read_variable_recursive(&mut self, var: u16, block: BlockId) -> Result<ValId, IrError> {
        let val = if !self.sealed.contains(&block) {
            let phi = self.new_phi(block, var)?;
            self.incomplete_phis.entry(block).or_default().push(phi);
            phi
        } else {
            let preds = self.preds.get(&block).cloned().unwrap_or_default();
            match preds.len() {
                0 => {
                    return Err(IrError::BadLocalRead {
                        index: var as usize,
                    })
                }
                1 => self.read_variable(var, preds[0])?,
                _ => {
                    let phi = self.new_phi(block, var)?;
                    self.write_variable(var, block, phi); // break cycles first
                    self.add_phi_operands(phi)?;
                    phi
                }
            }
        };
        self.write_variable(var, block, val);
        Ok(val)
    }

    fn new_phi(&mut self, block: BlockId, var: u16) -> Result<ValId, IrError> {
        let val = self.fresh()?;
        let idx = self.phis.len();
        self.phis.push(PhiDef {
            block,
            var,
            val,
            sources: Vec::new(),
        });
        self.phi_of.insert(val, idx);
        Ok(val)
    }

    fn add_phi_operands(&mut self, phi_val: ValId) -> Result<(), IrError> {
        let idx = self.phi_of[&phi_val];
        let (block, var) = (self.phis[idx].block, self.phis[idx].var);
        let preds = self.preds.get(&block).cloned().unwrap_or_default();
        for p in preds {
            let v = self.read_variable(var, p)?;
            self.phis[idx].sources.push((p, v));
        }
        Ok(())
    }

    fn seal_block(&mut self, block: BlockId) -> Result<(), IrError> {
        if let Some(list) = self.incomplete_phis.remove(&block) {
            for phi in list {
                self.add_phi_operands(phi)?;
            }
        }
        self.sealed.insert(block);
        Ok(())
    }

    // ---- node emission helpers (push onto the current block's node list) ----

    fn emit(
        &mut self,
        nodes: &mut Vec<Node>,
        stack: &mut Vec<ValId>,
        op: Op,
        ins: SmallVec<[ValId; 3]>,
        ty: Tag,
        effect: Effect,
    ) -> Result<(), IrError> {
        let id = self.fresh()?;
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
        &mut self,
        nodes: &mut Vec<Node>,
        stack: &mut Vec<ValId>,
        pc: u32,
        op: Op,
        ty: Tag,
        effect: Effect,
    ) -> Result<(), IrError> {
        let b = stack.pop().ok_or(IrError::StackUnderflow(pc))?;
        let a = stack.pop().ok_or(IrError::StackUnderflow(pc))?;
        self.emit(nodes, stack, op, smallvec![a, b], ty, effect)
    }

    fn unop(
        &mut self,
        nodes: &mut Vec<Node>,
        stack: &mut Vec<ValId>,
        pc: u32,
        op: Op,
        ty: Tag,
    ) -> Result<(), IrError> {
        let a = stack.pop().ok_or(IrError::StackUnderflow(pc))?;
        self.emit(nodes, stack, op, smallvec![a], ty, Effect::Pure)
    }

    fn konst(
        &mut self,
        nodes: &mut Vec<Node>,
        stack: &mut Vec<ValId>,
        val: Val128,
    ) -> Result<(), IrError> {
        let ty = val.tag();
        self.emit(nodes, stack, Op::Const(val), smallvec![], ty, Effect::Pure)
    }

    #[allow(clippy::too_many_arguments)]
    fn process_block(
        &mut self,
        block: BlockId,
        insns: &[Insn],
        cf: &ClassFile,
        cp: &ConstantPool,
        block_of: &HashMap<u32, BlockId>,
        nodes: &mut Vec<Node>,
    ) -> Result<Option<Terminator>, IrError> {
        let mut stack: Vec<ValId> = Vec::new();
        let mut term: Option<Terminator> = None;
        for ins in insns {
            let pc = ins.pc;
            match ins.op {
                0x00 => {}
                // constants
                0x02..=0x08 => {
                    self.konst(nodes, &mut stack, Val128::from_i32(ins.op as i32 - 3))?
                }
                0x09..=0x0a => {
                    self.konst(nodes, &mut stack, Val128::from_i64((ins.op - 0x09) as i64))?
                }
                0x0b..=0x0d => {
                    self.konst(nodes, &mut stack, Val128::from_f32((ins.op - 0x0b) as f32))?
                }
                0x10..=0x11 => self.konst(nodes, &mut stack, Val128::from_i32(ins.arg as i32))?,
                0x12..=0x14 => {
                    let v = cp_const(cp, ins.arg, pc)?;
                    self.konst(nodes, &mut stack, v)?;
                }
                // loads (read_variable)
                0x15..=0x17 => {
                    let v = self.read_variable(ins.arg as u16, block)?;
                    stack.push(v);
                }
                0x1a..=0x1d => {
                    let v = self.read_variable((ins.op - 0x1a) as u16, block)?;
                    stack.push(v);
                }
                0x1e..=0x21 => {
                    let v = self.read_variable((ins.op - 0x1e) as u16, block)?;
                    stack.push(v);
                }
                0x22..=0x25 => {
                    let v = self.read_variable((ins.op - 0x22) as u16, block)?;
                    stack.push(v);
                }
                // stores (write_variable)
                0x36..=0x38 => {
                    let v = stack.pop().ok_or(IrError::StackUnderflow(pc))?;
                    self.write_variable(ins.arg as u16, block, v);
                }
                0x3b..=0x3e => {
                    let v = stack.pop().ok_or(IrError::StackUnderflow(pc))?;
                    self.write_variable((ins.op - 0x3b) as u16, block, v);
                }
                0x3f..=0x42 => {
                    let v = stack.pop().ok_or(IrError::StackUnderflow(pc))?;
                    self.write_variable((ins.op - 0x3f) as u16, block, v);
                }
                0x43..=0x46 => {
                    let v = stack.pop().ok_or(IrError::StackUnderflow(pc))?;
                    self.write_variable((ins.op - 0x43) as u16, block, v);
                }
                // iinc index, const: local += const
                0x84 => {
                    let index = ((ins.arg >> 8) & 0xFF) as u16;
                    let delta = (ins.arg & 0xFF) as u8 as i8 as i32;
                    let cur = self.read_variable(index, block)?;
                    let k = self.fresh()?;
                    nodes.push(Node {
                        id: k,
                        op: Op::Const(Val128::from_i32(delta)),
                        ins: smallvec![],
                        ty: Tag::I32,
                        effect: Effect::Pure,
                    });
                    let r = self.fresh()?;
                    nodes.push(Node {
                        id: r,
                        op: Op::Add,
                        ins: smallvec![cur, k],
                        ty: Tag::I32,
                        effect: Effect::Pure,
                    });
                    self.write_variable(index, block, r);
                }
                // integer arithmetic / bitwise
                0x60 => self.binop(nodes, &mut stack, pc, Op::Add, Tag::I32, Effect::Pure)?,
                0x64 => self.binop(nodes, &mut stack, pc, Op::Sub, Tag::I32, Effect::Pure)?,
                0x68 => self.binop(nodes, &mut stack, pc, Op::Mul, Tag::I32, Effect::Pure)?,
                0x6c => self.binop(
                    nodes,
                    &mut stack,
                    pc,
                    Op::Div,
                    Tag::I32,
                    Effect::MayThrow { caught: false },
                )?,
                0x70 => self.binop(
                    nodes,
                    &mut stack,
                    pc,
                    Op::Rem,
                    Tag::I32,
                    Effect::MayThrow { caught: false },
                )?,
                0x74 => self.unop(nodes, &mut stack, pc, Op::Neg, Tag::I32)?,
                0x7e => self.binop(nodes, &mut stack, pc, Op::And, Tag::I32, Effect::Pure)?,
                // long arithmetic
                0x61 => self.binop(nodes, &mut stack, pc, Op::Add, Tag::I64, Effect::Pure)?,
                0x65 => self.binop(nodes, &mut stack, pc, Op::Sub, Tag::I64, Effect::Pure)?,
                0x69 => self.binop(nodes, &mut stack, pc, Op::Mul, Tag::I64, Effect::Pure)?,
                // float arithmetic
                0x62 => self.binop(nodes, &mut stack, pc, Op::Add, Tag::F32, Effect::Pure)?,
                0x66 => self.binop(nodes, &mut stack, pc, Op::Sub, Tag::F32, Effect::Pure)?,
                0x6a => self.binop(nodes, &mut stack, pc, Op::Mul, Tag::F32, Effect::Pure)?,
                // conversions
                0x85 => self.unop(nodes, &mut stack, pc, Op::Convert, Tag::I64)?,
                0x86 => self.unop(nodes, &mut stack, pc, Op::Convert, Tag::F32)?,
                0x88 => self.unop(nodes, &mut stack, pc, Op::Convert, Tag::I32)?,
                0x89 => self.unop(nodes, &mut stack, pc, Op::Convert, Tag::F32)?,
                0x8b => self.unop(nodes, &mut stack, pc, Op::Convert, Tag::I32)?,
                0x8c => self.unop(nodes, &mut stack, pc, Op::Convert, Tag::I64)?,
                // compares (3-way -> int)
                0x94..=0x95 => self.binop(
                    nodes,
                    &mut stack,
                    pc,
                    Op::Cmp { nan_greater: false },
                    Tag::I32,
                    Effect::Pure,
                )?,
                0x96 => self.binop(
                    nodes,
                    &mut stack,
                    pc,
                    Op::Cmp { nan_greater: true },
                    Tag::I32,
                    Effect::Pure,
                )?,
                // branches
                0x99..=0x9e => {
                    let v = stack.pop().ok_or(IrError::StackUnderflow(pc))?;
                    let cond = self.fresh()?;
                    nodes.push(Node {
                        id: cond,
                        op: Op::TestZero(if_cond(ins.op)),
                        ins: smallvec![v],
                        ty: Tag::I32,
                        effect: Effect::Pure,
                    });
                    term = Some(cond_branch(cond, ins, block_of, pc)?);
                }
                0x9f..=0xa4 => {
                    let b = stack.pop().ok_or(IrError::StackUnderflow(pc))?;
                    let a = stack.pop().ok_or(IrError::StackUnderflow(pc))?;
                    let cond = self.fresh()?;
                    nodes.push(Node {
                        id: cond,
                        op: Op::ICmp(if_icmp_cond(ins.op)),
                        ins: smallvec![a, b],
                        ty: Tag::I32,
                        effect: Effect::Pure,
                    });
                    term = Some(cond_branch(cond, ins, block_of, pc)?);
                }
                0xa7 => {
                    let taken = *block_of
                        .get(&ins.branch_target().unwrap())
                        .ok_or(IrError::BadBlock(pc))?;
                    term = Some(Terminator::Goto(taken));
                }
                0xac | 0xad => {
                    let v = stack.pop().ok_or(IrError::StackUnderflow(pc))?;
                    term = Some(Terminator::Return(Some(v)));
                }
                0xb8 => {
                    // invokestatic: pop the arguments (reverse), emit a call node, push the result.
                    let (mindex, arg_ty, ret_ty) = resolve_invokestatic(cf, ins.arg as u16, pc)?;
                    let mut args: SmallVec<[ValId; 3]> = SmallVec::new();
                    for _ in 0..arg_ty.len() {
                        args.push(stack.pop().ok_or(IrError::StackUnderflow(pc))?);
                    }
                    args.reverse();
                    let ty = match ret_ty {
                        None | Some(VType::Int) => Tag::I32, // void: placeholder, never pushed
                        Some(VType::Long) => Tag::I64,
                        Some(VType::Float) => Tag::F32,
                        Some(VType::Double) => Tag::F64,
                        Some(_) => return Err(IrError::Unsupported { op: 0xb8, pc }),
                    };
                    let id = self.fresh()?;
                    nodes.push(Node {
                        id,
                        op: Op::InvokeStatic(mindex),
                        ins: args,
                        ty,
                        effect: Effect::Extern,
                    });
                    if ret_ty.is_some() {
                        stack.push(id);
                    }
                }
                _ => return Err(IrError::Unsupported { op: ins.op, pc }),
            }
        }
        Ok(term)
    }
}

fn cond_branch(
    cond: ValId,
    ins: &Insn,
    block_of: &HashMap<u32, BlockId>,
    pc: u32,
) -> Result<Terminator, IrError> {
    let taken = *block_of
        .get(&ins.branch_target().unwrap())
        .ok_or(IrError::BadBlock(pc))?;
    let not_taken = *block_of
        .get(&(pc + ins.len as u32))
        .ok_or(IrError::BadBlock(pc))?;
    Ok(Terminator::CondBranch {
        cond,
        taken,
        not_taken,
    })
}

/// Successors of a block, from its last instruction.
fn successors(
    last: Option<&Insn>,
    bidx: usize,
    nblocks: usize,
    block_of: &HashMap<u32, BlockId>,
) -> Vec<BlockId> {
    let next = |bidx: usize| -> Vec<BlockId> {
        if bidx + 1 < nblocks {
            vec![BlockId((bidx + 1) as u32)]
        } else {
            vec![]
        }
    };
    match last {
        Some(ins) if (0x99..=0xa4).contains(&ins.op) => {
            let mut s = Vec::new();
            if let Some(t) = ins.branch_target().and_then(|t| block_of.get(&t)) {
                s.push(*t);
            }
            if let Some(&fb) = block_of.get(&(ins.pc + ins.len as u32)) {
                s.push(fb);
            }
            s
        }
        Some(ins) if ins.op == 0xa7 => ins
            .branch_target()
            .and_then(|t| block_of.get(&t))
            .map(|b| vec![*b])
            .unwrap_or_default(),
        Some(ins) if is_return(ins.op) => vec![],
        _ => next(bidx),
    }
}

/// Reverse post-order of the reachable blocks from `entry`.
fn reverse_postorder(
    entry: BlockId,
    succs: &HashMap<BlockId, Vec<BlockId>>,
    n: usize,
) -> Vec<BlockId> {
    let mut visited = vec![false; n];
    let mut post = Vec::new();
    let mut stack = vec![(entry, 0usize)];
    visited[entry.0 as usize] = true;
    while let Some(&(b, i)) = stack.last() {
        let s = succs.get(&b).map(Vec::as_slice).unwrap_or(&[]);
        if i < s.len() {
            stack.last_mut().unwrap().1 += 1;
            let c = s[i];
            if !visited[c.0 as usize] {
                visited[c.0 as usize] = true;
                stack.push((c, 0));
            }
        } else {
            post.push(b);
            stack.pop();
        }
    }
    post.reverse();
    post
}

/// Build the L1 SSA form of a verified method.
pub fn build(vm: &VerifiedMethod, cf: &ClassFile) -> Result<BuiltMethod, IrError> {
    let cp = &cf.constant_pool;
    let valid: HashMap<u32, usize> = vm
        .insns
        .iter()
        .enumerate()
        .map(|(i, ins)| (ins.pc, i))
        .collect();
    let leader_pcs = leaders(&vm.insns, &valid);
    let nblocks = leader_pcs.len();
    let block_of: HashMap<u32, BlockId> = leader_pcs
        .iter()
        .enumerate()
        .map(|(i, &pc)| (pc, BlockId(i as u32)))
        .collect();

    // Partition instructions into blocks (already in pc order).
    let mut block_insns: Vec<Vec<Insn>> = vec![Vec::new(); nblocks];
    let mut bi = 0;
    for ins in &vm.insns {
        while bi + 1 < nblocks && ins.pc >= leader_pcs[bi + 1] {
            bi += 1;
        }
        block_insns[bi].push(*ins);
    }

    // CFG edges.
    let mut succs: HashMap<BlockId, Vec<BlockId>> = HashMap::new();
    let mut preds: HashMap<BlockId, Vec<BlockId>> = HashMap::new();
    for (idx, insns) in block_insns.iter().enumerate() {
        let s = successors(insns.last(), idx, nblocks, &block_of);
        for &t in &s {
            preds.entry(t).or_default().push(BlockId(idx as u32));
        }
        succs.insert(BlockId(idx as u32), s);
    }

    // If the very first block is itself a loop header (has a back-edge), insert a synthetic entry
    // block that merely jumps to it, so the real first block gains a non-back-edge predecessor and
    // its loop-carried locals get proper φ (otherwise arguments bound at the entry would shadow the
    // φ and the loop would never observe updated values — e.g. a tight `while` at pc 0).
    let needs_synth = preds.get(&BlockId(0)).is_some_and(|p| !p.is_empty());
    let total = if needs_synth { nblocks + 1 } else { nblocks };
    let entry = if needs_synth {
        let synth = BlockId(nblocks as u32);
        succs.insert(synth, vec![BlockId(0)]);
        preds.entry(BlockId(0)).or_default().push(synth);
        synth
    } else {
        BlockId(0)
    };
    let order = reverse_postorder(entry, &succs, total);

    let mut ssa = Ssa {
        current_def: HashMap::new(),
        phis: Vec::new(),
        phi_of: HashMap::new(),
        incomplete_phis: HashMap::new(),
        sealed: HashSet::new(),
        preds,
        next: 0,
    };

    // Entry has no predecessors: seal it, then bind arguments.
    ssa.seal_block(entry)?;
    let mut arg_vals = Vec::new();
    {
        let mut slot = 0u16;
        if !vm.is_static {
            let v = ssa.fresh()?;
            ssa.write_variable(slot, entry, v);
            arg_vals.push(v);
            slot += 1;
        }
        for &t in &vm.arg_types {
            let v = ssa.fresh()?;
            ssa.write_variable(slot, entry, v);
            arg_vals.push(v);
            slot += t.size();
        }
    }

    // Process blocks in RPO, sealing each as soon as all its predecessors are processed.
    let mut nodes_of: Vec<Vec<Node>> = (0..total).map(|_| Vec::new()).collect();
    let mut term_of: Vec<Option<Terminator>> = (0..total).map(|_| None).collect();
    let mut processed: HashSet<BlockId> = HashSet::new();
    let all_processed = |b: BlockId, ssa: &Ssa, processed: &HashSet<BlockId>| {
        ssa.preds
            .get(&b)
            .is_none_or(|ps| ps.iter().all(|p| processed.contains(p)))
    };

    for &b in &order {
        if !ssa.sealed.contains(&b) && all_processed(b, &ssa, &processed) {
            ssa.seal_block(b)?;
        }
        let bidx = b.0 as usize;
        if bidx < nblocks {
            let mut nodes = Vec::new();
            let term = ssa.process_block(b, &block_insns[bidx], cf, cp, &block_of, &mut nodes)?;
            nodes_of[bidx] = nodes;
            term_of[bidx] = Some(term.unwrap_or_else(|| {
                // Fall-through into the next block in pc order.
                Terminator::Goto(BlockId((bidx + 1).min(nblocks.saturating_sub(1)) as u32))
            }));
        } else {
            // Synthetic entry block: jump to the real first block.
            term_of[bidx] = Some(Terminator::Goto(BlockId(0)));
        }
        processed.insert(b);
        // Seal any block whose predecessors are now all processed (e.g. a loop header after its
        // back-edge block has been processed).
        let to_seal: Vec<BlockId> = order
            .iter()
            .copied()
            .filter(|&hb| !ssa.sealed.contains(&hb) && all_processed(hb, &ssa, &processed))
            .collect();
        for hb in to_seal {
            ssa.seal_block(hb)?;
        }
    }

    // Group φ definitions by block.
    let mut phis_of: Vec<Vec<Phi>> = (0..total).map(|_| Vec::new()).collect();
    for pd in &ssa.phis {
        phis_of[pd.block.0 as usize].push(Phi {
            slot: pd.val,
            sources: pd.sources.iter().copied().collect(),
        });
    }

    let mut blocks = Vec::with_capacity(total);
    for idx in 0..total {
        blocks.push(Block {
            id: BlockId(idx as u32),
            phis: std::mem::take(&mut phis_of[idx]),
            nodes: std::mem::take(&mut nodes_of[idx]),
            term: term_of[idx].take().unwrap_or(Terminator::Return(None)),
        });
    }

    Ok(BuiltMethod {
        method: Method {
            blocks,
            entry,
            max_locals: vm.max_locals,
            exc_table: Vec::new(),
        },
        arg_vals,
        n_slots: ssa.next as usize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(name: &str) -> Option<Vec<u8>> {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../testdata/java/{name}.java"));
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
        std::fs::read(out.join(format!("{name}.class"))).ok()
    }

    #[test]
    fn builds_loop_with_phi() {
        let Some(bytes) = compile("Loops") else {
            eprintln!("skipping builds_loop_with_phi: javac unavailable");
            return;
        };
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let m = cf.method("sumTo", "(I)I").unwrap();
        let vm = rjava_verify::verify_method(&cf, m).unwrap();
        let built = build(&vm, &cf).unwrap();
        // The loop header must carry φ nodes for the loop-carried locals (s, i).
        let total_phis: usize = built.method.blocks.iter().map(|b| b.phis.len()).sum();
        assert!(
            total_phis >= 2,
            "loop header should have φ for s and i, got {total_phis}"
        );
        assert_eq!(built.arg_vals.len(), 1); // n
    }
}
