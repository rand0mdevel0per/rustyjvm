//! rjava-verify — JVMS §4.10 bytecode verification via the type-checking (`StackMapTable`) verifier
//! (RJVM-SPEC-001 §7). Verification is the security floor (§7.1): it runs before flattening and
//! rejects any type-unsafe or malformed method with [`VerifyError`], never panicking on adversarial
//! input (§23.3). It also produces the static type facts (`VerifiedMethod`) that `rjava-ir`
//! consumes to build SSA.
//!
//! Increment 1 covers the vertical-slice opcode set (arithmetic/convert/compare/branch over
//! int/long/float, `ireturn`); other opcodes are rejected as unsupported and added per increment.

mod error;
mod insn;
mod stackmap;
mod vtype;

use std::collections::BTreeMap;

use rjava_classfile::{ClassFile, Constant, ConstantPool, MemberInfo};

pub use error::VerifyError;
pub use insn::{decode, pc_index, Insn};
pub use stackmap::Frame;
pub use vtype::{is_assignable, parse_method_descriptor, VType};

const ACC_STATIC: u16 = 0x0008;

/// A verified method: decoded instructions plus the static facts SSA construction needs.
#[derive(Debug, Clone)]
pub struct VerifiedMethod {
    pub insns: Vec<Insn>,
    pub max_stack: u16,
    pub max_locals: u16,
    pub is_static: bool,
    /// Argument types in declaration order, item model (`long`/`double` = one entry).
    pub arg_types: Vec<VType>,
    /// Return type, or `None` for `void`.
    pub ret: Option<VType>,
    /// The `StackMapTable`-declared frame at each offset (empty if the method has no branches).
    pub frames: BTreeMap<u32, Frame>,
}

/// Verify a method per JVMS §4.10, returning the facts needed downstream.
pub fn verify_method(cf: &ClassFile, m: &MemberInfo) -> Result<VerifiedMethod, VerifyError> {
    let code = m.code().ok_or(VerifyError::NoCode)?;
    let is_static = m.access_flags & ACC_STATIC != 0;
    let descriptor = m
        .descriptor(&cf.constant_pool)
        .ok_or(VerifyError::BadDescriptor)?;
    let (arg_types, ret) = parse_method_descriptor(descriptor)?;

    let insns = decode(&code.code)?;
    let index = pc_index(&insns);
    // Branch targets MUST land on an instruction boundary (JVMS §4.9.1).
    for ins in &insns {
        if let Some(t) = ins.branch_target() {
            if !index.contains_key(&t) {
                return Err(VerifyError::BadBranchTarget {
                    at: ins.pc,
                    target: t as i64,
                });
            }
        }
    }

    // Initial locals in the item model: `this` (if instance) then the arguments.
    let mut initial_items = Vec::new();
    if !is_static {
        initial_items.push(VType::Reference);
    }
    initial_items.extend_from_slice(&arg_types);

    // Decode StackMapTable (if present) using the item-model initial locals.
    let frames = match code
        .attributes
        .iter()
        .find_map(|a| a.raw_named(&cf.constant_pool, "StackMapTable"))
    {
        Some(smt) => stackmap::decode(smt, &initial_items)?,
        None => BTreeMap::new(),
    };

    // Initial abstract frame: slot-model locals padded to max_locals with Top; empty stack.
    let mut locals = expand_to_slots(&initial_items);
    if locals.len() > code.max_locals as usize {
        return Err(VerifyError::BadLocal {
            pc: 0,
            index: code.max_locals,
        });
    }
    locals.resize(code.max_locals as usize, VType::Top);
    let initial = AbstractFrame {
        locals,
        stack: Vec::new(),
    };

    typecheck(
        &insns,
        &frames,
        initial,
        code.max_stack,
        code.max_locals,
        ret,
        &cf.constant_pool,
    )?;

    Ok(VerifiedMethod {
        insns,
        max_stack: code.max_stack,
        max_locals: code.max_locals,
        is_static,
        arg_types,
        ret,
        frames,
    })
}

fn expand_to_slots(items: &[VType]) -> Vec<VType> {
    let mut out = Vec::with_capacity(items.len());
    for &t in items {
        out.push(t);
        if t.is_category2() {
            out.push(VType::Top);
        }
    }
    out
}

/// Abstract interpreter state: slot-model locals + item-model operand stack.
#[derive(Clone)]
struct AbstractFrame {
    locals: Vec<VType>,
    stack: Vec<VType>,
}

impl AbstractFrame {
    fn slot_depth(&self) -> u16 {
        self.stack.iter().map(|t| t.size()).sum()
    }
    fn push(&mut self, t: VType, max_stack: u16, pc: u32) -> Result<(), VerifyError> {
        if self.slot_depth() + t.size() > max_stack {
            return Err(VerifyError::StackOverflow(pc));
        }
        self.stack.push(t);
        Ok(())
    }
    fn pop_expect(&mut self, want: VType, pc: u32) -> Result<(), VerifyError> {
        let got = self.stack.pop().ok_or(VerifyError::StackUnderflow(pc))?;
        if is_assignable(got, want) {
            Ok(())
        } else {
            Err(VerifyError::TypeMismatch {
                pc,
                what: "operand type",
            })
        }
    }
    /// Reconstruct an abstract frame from a declared `StackMapTable` frame, padding locals to
    /// `max_locals` with `Top`.
    fn from_declared(declared: &Frame, max_locals: u16) -> Result<AbstractFrame, VerifyError> {
        if declared.locals.len() > max_locals as usize {
            return Err(VerifyError::BadStackMap);
        }
        let mut locals = declared.locals.clone();
        locals.resize(max_locals as usize, VType::Top);
        Ok(AbstractFrame {
            locals,
            stack: declared.stack.clone(),
        })
    }
    /// Whether this (outgoing) state satisfies a declared frame at a merge/target.
    fn assignable_to(&self, declared: &Frame) -> bool {
        if declared.locals.len() > self.locals.len() || self.stack.len() != declared.stack.len() {
            return false;
        }
        declared
            .locals
            .iter()
            .zip(&self.locals)
            .all(|(&d, &c)| is_assignable(c, d))
            && self
                .stack
                .iter()
                .zip(&declared.stack)
                .all(|(&c, &d)| is_assignable(c, d))
    }
}

#[allow(clippy::too_many_arguments)]
fn typecheck(
    insns: &[Insn],
    frames: &BTreeMap<u32, Frame>,
    initial: AbstractFrame,
    max_stack: u16,
    max_locals: u16,
    ret: Option<VType>,
    cp: &ConstantPool,
) -> Result<(), VerifyError> {
    let mut cur: Option<AbstractFrame> = Some(initial);
    for ins in insns {
        // Reconcile with a declared frame at this offset (branch target / merge point).
        if let Some(declared) = frames.get(&ins.pc) {
            if let Some(c) = &cur {
                if !c.assignable_to(declared) {
                    return Err(VerifyError::FrameMismatch(ins.pc));
                }
            }
            cur = Some(AbstractFrame::from_declared(declared, max_locals)?);
        }
        {
            let frame = cur.as_mut().ok_or(VerifyError::MissingFrame(ins.pc))?;
            apply(frame, ins, cp, max_stack, max_locals, ret)?;
            if let Some(target) = ins.branch_target() {
                let declared = frames
                    .get(&target)
                    .ok_or(VerifyError::MissingFrame(target))?;
                if !frame.assignable_to(declared) {
                    return Err(VerifyError::FrameMismatch(target));
                }
            }
        }
        if ins.is_unconditional_end() {
            cur = None; // goto / return: no fall-through
        }
    }
    Ok(())
}

fn ldc_type(cp: &ConstantPool, index: i64, pc: u32) -> Result<VType, VerifyError> {
    let idx = u16::try_from(index).map_err(|_| VerifyError::TypeMismatch {
        pc,
        what: "ldc index",
    })?;
    match cp.get(idx) {
        Some(Constant::Integer(_)) => Ok(VType::Int),
        Some(Constant::Float(_)) => Ok(VType::Float),
        Some(Constant::Long(_)) => Ok(VType::Long),
        Some(Constant::Double(_)) => Ok(VType::Double),
        Some(Constant::String { .. }) | Some(Constant::Class { .. }) => Ok(VType::Reference),
        _ => Err(VerifyError::TypeMismatch {
            pc,
            what: "ldc constant kind",
        }),
    }
}

fn load(
    f: &mut AbstractFrame,
    idx: u16,
    want: VType,
    max_stack: u16,
    max_locals: u16,
    pc: u32,
) -> Result<(), VerifyError> {
    let hi = idx as usize + want.is_category2() as usize;
    if hi >= max_locals as usize {
        return Err(VerifyError::BadLocal { pc, index: idx });
    }
    if f.locals[idx as usize] != want {
        return Err(VerifyError::TypeMismatch {
            pc,
            what: "local type",
        });
    }
    if want.is_category2() && f.locals[idx as usize + 1] != VType::Top {
        return Err(VerifyError::TypeMismatch {
            pc,
            what: "category-2 local half",
        });
    }
    f.push(want, max_stack, pc)
}

fn store(
    f: &mut AbstractFrame,
    idx: u16,
    t: VType,
    max_locals: u16,
    pc: u32,
) -> Result<(), VerifyError> {
    let hi = idx as usize + t.is_category2() as usize;
    if hi >= max_locals as usize {
        return Err(VerifyError::BadLocal { pc, index: idx });
    }
    f.pop_expect(t, pc)?;
    f.locals[idx as usize] = t;
    if t.is_category2() {
        f.locals[idx as usize + 1] = VType::Top;
    }
    Ok(())
}

/// Apply one instruction's type transition to the frame (JVMS §4.10.1.6 style, slice subset).
fn apply(
    f: &mut AbstractFrame,
    ins: &Insn,
    cp: &ConstantPool,
    max_stack: u16,
    max_locals: u16,
    ret: Option<VType>,
) -> Result<(), VerifyError> {
    use VType::{Float, Int, Long};
    let pc = ins.pc;
    match ins.op {
        0x00 => {} // nop
        // ---- constants ----
        0x02..=0x08 | 0x10 | 0x11 => f.push(Int, max_stack, pc)?, // iconst_*/bipush/sipush
        0x09 | 0x0a => f.push(Long, max_stack, pc)?,              // lconst_0/1
        0x0b..=0x0d => f.push(Float, max_stack, pc)?,             // fconst_0/1/2
        0x12 | 0x13 => {
            let t = ldc_type(cp, ins.arg, pc)?;
            if t.is_category2() {
                return Err(VerifyError::TypeMismatch {
                    pc,
                    what: "ldc of category-2 constant",
                });
            }
            f.push(t, max_stack, pc)?;
        }
        0x14 => {
            let t = ldc_type(cp, ins.arg, pc)?;
            if !t.is_category2() {
                return Err(VerifyError::TypeMismatch {
                    pc,
                    what: "ldc2_w of category-1 constant",
                });
            }
            f.push(t, max_stack, pc)?;
        }
        // ---- loads ----
        0x15 => load(f, ins.arg as u16, Int, max_stack, max_locals, pc)?,
        0x1a..=0x1d => load(f, (ins.op - 0x1a) as u16, Int, max_stack, max_locals, pc)?,
        0x16 => load(f, ins.arg as u16, Long, max_stack, max_locals, pc)?,
        0x1e..=0x21 => load(f, (ins.op - 0x1e) as u16, Long, max_stack, max_locals, pc)?,
        0x17 => load(f, ins.arg as u16, Float, max_stack, max_locals, pc)?,
        0x22..=0x25 => load(f, (ins.op - 0x22) as u16, Float, max_stack, max_locals, pc)?,
        // ---- stores ----
        0x36 => store(f, ins.arg as u16, Int, max_locals, pc)?,
        0x3b..=0x3e => store(f, (ins.op - 0x3b) as u16, Int, max_locals, pc)?,
        0x37 => store(f, ins.arg as u16, Long, max_locals, pc)?,
        0x3f..=0x42 => store(f, (ins.op - 0x3f) as u16, Long, max_locals, pc)?,
        0x38 => store(f, ins.arg as u16, Float, max_locals, pc)?,
        0x43..=0x46 => store(f, (ins.op - 0x43) as u16, Float, max_locals, pc)?,
        // ---- arithmetic (typed by opcode) ----
        0x60 | 0x64 | 0x68 | 0x6c | 0x70 => {
            f.pop_expect(Int, pc)?;
            f.pop_expect(Int, pc)?;
            f.push(Int, max_stack, pc)?;
        } // iadd/isub/imul/idiv/irem
        0x74 => {
            f.pop_expect(Int, pc)?;
            f.push(Int, max_stack, pc)?;
        } // ineg
        0x7e => {
            f.pop_expect(Int, pc)?;
            f.pop_expect(Int, pc)?;
            f.push(Int, max_stack, pc)?;
        } // iand
        0x84 => {
            // iinc: local += const; the local must be an int and stays one (no stack effect).
            let idx = ((ins.arg >> 8) & 0xFF) as u16;
            if idx as usize >= max_locals as usize {
                return Err(VerifyError::BadLocal { pc, index: idx });
            }
            if f.locals[idx as usize] != Int {
                return Err(VerifyError::TypeMismatch {
                    pc,
                    what: "iinc on non-int local",
                });
            }
        } // iinc
        0x61 | 0x65 | 0x69 => {
            f.pop_expect(Long, pc)?;
            f.pop_expect(Long, pc)?;
            f.push(Long, max_stack, pc)?;
        } // ladd/lsub/lmul
        0x62 | 0x66 | 0x6a => {
            f.pop_expect(Float, pc)?;
            f.pop_expect(Float, pc)?;
            f.push(Float, max_stack, pc)?;
        } // fadd/fsub/fmul
        // ---- conversions ----
        0x85 => {
            f.pop_expect(Int, pc)?;
            f.push(Long, max_stack, pc)?;
        } // i2l
        0x86 => {
            f.pop_expect(Int, pc)?;
            f.push(Float, max_stack, pc)?;
        } // i2f
        0x88 => {
            f.pop_expect(Long, pc)?;
            f.push(Int, max_stack, pc)?;
        } // l2i
        0x89 => {
            f.pop_expect(Long, pc)?;
            f.push(Float, max_stack, pc)?;
        } // l2f
        0x8b => {
            f.pop_expect(Float, pc)?;
            f.push(Int, max_stack, pc)?;
        } // f2i
        0x8c => {
            f.pop_expect(Float, pc)?;
            f.push(Long, max_stack, pc)?;
        } // f2l
        // ---- compares ----
        0x94 => {
            f.pop_expect(Long, pc)?;
            f.pop_expect(Long, pc)?;
            f.push(Int, max_stack, pc)?;
        } // lcmp
        0x95 | 0x96 => {
            f.pop_expect(Float, pc)?;
            f.pop_expect(Float, pc)?;
            f.push(Int, max_stack, pc)?;
        } // fcmpl/fcmpg
        // ---- branches ----
        0x99..=0x9e => f.pop_expect(Int, pc)?, // if<cond> (pops one int)
        0x9f..=0xa4 => {
            f.pop_expect(Int, pc)?;
            f.pop_expect(Int, pc)?;
        } // if_icmp<cond>
        0xa7 => {}                             // goto
        // ---- return ----
        0xac => {
            f.pop_expect(Int, pc)?;
            if ret != Some(Int) {
                return Err(VerifyError::BadReturn(pc));
            }
        } // ireturn
        0xad => {
            f.pop_expect(Long, pc)?;
            if ret != Some(Long) {
                return Err(VerifyError::BadReturn(pc));
            }
        } // lreturn
        _ => return Err(VerifyError::UnsupportedOpcode { op: ins.op, pc }),
    }
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
        let out = std::env::temp_dir().join(format!("rjava-vf-{}", std::process::id()));
        std::fs::create_dir_all(&out).ok()?;
        let status = std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&out)
            .arg(&src)
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        std::fs::read(out.join("Slice.class")).ok()
    }

    #[test]
    fn verifies_slice_arith() {
        let Some(bytes) = compile_slice() else {
            eprintln!("skipping verifies_slice_arith: javac unavailable");
            return;
        };
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let arith = cf.method("arith", "(IIJF)I").unwrap();
        let vm = verify_method(&cf, arith).expect("arith must verify");
        assert_eq!(vm.max_locals, 9);
        assert_eq!(vm.ret, Some(VType::Int));
        assert_eq!(
            vm.arg_types,
            vec![VType::Int, VType::Int, VType::Long, VType::Float]
        );
        // Two StackMapTable frames (at pc 63 and 76 per javap).
        assert_eq!(vm.frames.len(), 2);
        assert!(vm.frames.contains_key(&63));
        assert!(vm.frames.contains_key(&76));
        // <init> uses aload_0/invokespecial/return — outside the increment-1 opcode set — so it
        // must be rejected as unsupported (reject-unknown), not mis-verified.
        let init = cf.method("<init>", "()V").unwrap();
        assert!(matches!(
            verify_method(&cf, init),
            Err(VerifyError::UnsupportedOpcode { .. })
        ));
    }

    #[test]
    fn rejects_stack_underflow() {
        // A single `iadd` on an empty stack must be rejected, not panic. `apply` never touches the
        // constant pool for arithmetic, so a minimal dummy pool suffices.
        let insns = vec![Insn {
            pc: 0,
            op: 0x60,
            arg: 0,
            len: 1,
        }];
        let initial = AbstractFrame {
            locals: vec![VType::Top; 1],
            stack: vec![],
        };
        let frames = BTreeMap::new();
        let cp = make_dummy_cp();
        let r = typecheck(&insns, &frames, initial, 2, 1, Some(VType::Int), &cp);
        assert_eq!(r, Err(VerifyError::StackUnderflow(0)));
    }

    /// A constant pool with only the index-0 sentinel, for tests that never index it.
    fn make_dummy_cp() -> ConstantPool {
        // Magic + version + constant_pool_count=1 (no entries) + minimal trailer to satisfy parse.
        // Simpler: parse a tiny hand-built class is heavy; instead round-trip through parse of a
        // 1-count pool embedded in a minimal class.
        let bytes = minimal_class_bytes();
        rjava_classfile::parse(&bytes).unwrap().constant_pool
    }

    /// Bytes of a minimal valid class: magic, v65, cp_count=1, flags, this=0? -> we only need the
    /// constant pool to parse; downstream fields can be zero/empty.
    fn minimal_class_bytes() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&0xCAFE_BABEu32.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes()); // minor
        b.extend_from_slice(&65u16.to_be_bytes()); // major
        b.extend_from_slice(&1u16.to_be_bytes()); // constant_pool_count = 1 (no entries)
        b.extend_from_slice(&0u16.to_be_bytes()); // access_flags
        b.extend_from_slice(&0u16.to_be_bytes()); // this_class
        b.extend_from_slice(&0u16.to_be_bytes()); // super_class
        b.extend_from_slice(&0u16.to_be_bytes()); // interfaces_count
        b.extend_from_slice(&0u16.to_be_bytes()); // fields_count
        b.extend_from_slice(&0u16.to_be_bytes()); // methods_count
        b.extend_from_slice(&0u16.to_be_bytes()); // attributes_count
        b
    }

    proptest::proptest! {
        /// Fuzz: no random byte string may make parse+verify panic (§23.3).
        #[test]
        fn parse_then_verify_never_panics(bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..300)) {
            if let Ok(cf) = rjava_classfile::parse(&bytes) {
                for m in &cf.methods {
                    let _ = verify_method(&cf, m);
                }
            }
        }
    }
}
