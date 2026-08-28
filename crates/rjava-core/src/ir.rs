//! Intermediate-representation datatypes (RJVM-SPEC-001 §9). The L1 SSA data-flow tree (§9.2) is
//! built by `rjava-ir`; these are the shared datatypes it produces and that `rjava-interp`
//! linearises into the L2 op-stream (§9.3). `Node.ins` IS the dependency set that drives early /
//! out-of-order issue (§10) — no separate analysis.

use portable_atomic::AtomicU32;
use smallvec::SmallVec;

use crate::diff::EscapeState;
use crate::ids::{BlockId, ClassId, SlotId};
use crate::value::{Tag, Val128};

/// Two-level SSA value id (§9.1). `scope_level` is fixed by super-scope nesting and never drifts
/// across block reordering (stable cross-block references); `offset` is reusable within a level
/// (compactness). `offset <= 1024`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct ValId {
    pub scope_level: u16,
    pub offset: u16,
}

/// Side-effect classification driving speculation eligibility (§9.2, §10.6).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    /// Free to reorder / hoist / speculate.
    Pure,
    /// Constrained by store dependencies.
    ReadHeap,
    /// A GC/JMM barrier point (§6.5).
    WriteHeap,
    /// Non-intrinsic (native/IO): fenced, lands in program order (§10.6).
    Extern,
    /// Can throw. `caught = true` installs an implicit control edge to a local handler (§20.3).
    MayThrow { caught: bool },
}

/// Integer comparison against zero — the condition of a JVM `if<cond>` branch (JVMS §6.5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IntCond {
    Eq,
    Ne,
    Lt,
    Ge,
    Gt,
    Le,
}

/// SSA operation kind. Typed by [`Node::ty`], so a single arithmetic variant serves int/long/float
/// (§9.2 — "ty is the static type fixed by verification"). Extended per increment; this is the
/// increment-1 arithmetic / convert / compare core.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    /// Materialise a constant.
    Const(Val128),
    Add,
    Sub,
    Mul,
    /// Integer div/rem trap on a zero divisor (`Effect::MayThrow`); float div/rem never trap.
    Div,
    Rem,
    Neg,
    /// Numeric conversion; result type is [`Node::ty`], source type is the input node's `ty`.
    Convert,
    /// Three-way compare producing int -1/0/1 (`lcmp`, `fcmp<op>`). `nan_greater` selects `fcmpg`
    /// (true) versus `fcmpl` (false); ignored for `lcmp` (longs cannot be NaN).
    Cmp {
        nan_greater: bool,
    },
    /// `(input <cond> 0) ? 1 : 0`, as int. Produced for `if<cond>` so a nonzero-testing
    /// [`Terminator::CondBranch`] can express the JVM branch condition.
    TestZero(IntCond),
    /// `(reference is null) == expect_null ? 1 : 0`, as int. Produced for `ifnull`/`ifnonnull`.
    TestNull {
        expect_null: bool,
    },
    /// `(a is the same object as b) == expect_same ? 1 : 0`, as int. Produced for
    /// `if_acmpeq`/`if_acmpne`: reference **identity**, which is what makes string interning
    /// observable (§18.4).
    RefEq {
        expect_same: bool,
    },
    /// `(a <cond> b) ? 1 : 0`, as int (two operands). Produced for `if_icmp<cond>`.
    ICmp(IntCond),
    /// Bitwise AND (typed by [`Node::ty`]: int or long). Other bitwise/shift ops are added on
    /// demand in later increments.
    And,
    // ---- symbolic references (§8.3) ----
    // These carry a *constant-pool index*, not a resolved target: class identity, field layout and
    // dispatch are runtime-determined and must not be baked at build time (P-3, §8.3, §13.4). The
    // interpreter resolves them through the class registry at first use.
    /// `invokestatic`. `ins` are the arguments in order; `ty` is the return type (unused for `void`).
    InvokeStatic(u16),
    /// `invokespecial` — a constructor or other non-virtual instance call. `ins` is `[this, args…]`.
    InvokeSpecial(u16),
    /// `invokevirtual` — dispatched on the *runtime* class of `ins[0]` through the mutable dispatch
    /// table (§13.4), never on the statically named class.
    InvokeVirtual(u16),
    /// Allocate an instance of the class named by the constant-pool entry. `escape` is the
    /// escape-analysis classification (§9.4); `ins` is empty — allocation has no prerequisite
    /// dependencies (§22.1).
    New {
        class_cp: u16,
        escape: EscapeState,
    },
    /// Read the instance field named by the constant-pool entry from `ins[0]`; `ty` is its type.
    GetField(u16),
    /// Write `ins[1]` into the instance field named by the constant-pool entry of `ins[0]`.
    PutField(u16),
    /// Materialise the interned `java.lang.String` for a `String` constant (`ldc`). Interning is
    /// observable: two `ldc` of the same literal yield the *same* reference (§18.4).
    LoadString(u16),
    /// Read a static field; triggers initialisation of its declaring class (§8.5).
    GetStatic(u16),
    /// Write `ins[0]` to a static field; triggers initialisation of its declaring class (§8.5).
    PutStatic(u16),
    /// `instanceof`: 1 if `ins[0]` is a non-null instance of the named class, else 0.
    InstanceOf(u16),
    /// `checkcast`: passes `ins[0]` through, raising `ClassCastException` if it is not an instance
    /// of the named class (increment 8 turns that into a real Java exception).
    CheckCast(u16),
}

/// An SSA node: a single value definition (§9.2).
pub struct Node {
    /// The SSA value this node defines.
    pub id: ValId,
    pub op: Op,
    /// Def-use edges = dependencies. The scheduler issues a node once its `ins` are ready (§9.2).
    pub ins: SmallVec<[ValId; 3]>,
    /// Static type fixed by verification; feeds JIT unboxing (§9.2, §13.5).
    pub ty: Tag,
    pub effect: Effect,
}

/// A control-flow merge slot (§9.2). Maps 1:1 to a backend `phi`.
pub struct Phi {
    /// The merge slot (this scope_level, fixed offset).
    pub slot: ValId,
    pub sources: SmallVec<[(BlockId, ValId); 2]>,
}

/// Basic-block terminator.
pub enum Terminator {
    /// Return, optionally with a value.
    Return(Option<ValId>),
    /// Unconditional branch.
    Goto(BlockId),
    /// Branch on an int condition value being non-zero.
    CondBranch {
        cond: ValId,
        taken: BlockId,
        not_taken: BlockId,
    },
    /// Throw an exception object.
    Throw(ValId),
}

/// A basic block (§9.2).
pub struct Block {
    pub id: BlockId,
    pub phis: Vec<Phi>,
    pub nodes: Vec<Node>,
    pub term: Terminator,
}

/// Exception-handler coverage over a range of blocks (§9.2, §20.6).
pub struct ExcRegion {
    pub start: BlockId,
    pub end: BlockId,
    pub handler: BlockId,
    /// `None` = catch-all (`finally`).
    pub catch_type: Option<ClassId>,
}

/// A verified L1 method body (§9.2).
pub struct Method {
    pub blocks: Vec<Block>,
    pub entry: BlockId,
    /// `<= 1024` per scope (§9.5).
    pub max_locals: u16,
    pub exc_table: Vec<ExcRegion>,
}

// ---- L2: the runtime op-stream (§9.3) ----

/// Interpret this op (no acceleration installed).
pub const ACCEL_INTERPRET: u32 = 0;
/// Async JIT compilation is in progress for this op's fragment.
pub const ACCEL_JIT_PENDING: u32 = 1;
/// First value denoting a compiled-blob id; `accel >= ACCEL_BLOB_BASE` selects native code.
pub const ACCEL_BLOB_BASE: u32 = 2;

/// A linear op-stream instruction (§9.3). Dispatch is an `opid`-indexed computed jump
/// (`base + opid * stride`). `accel` is the single-word fragment-replacement seam shared by JIT,
/// AOT, and JNI thunks (§9.3, §13.3, §16.1): CAS it from an indicator to a compiled-blob id. This
/// seam is pinned now so acceleration layers are pure additions later (P-1).
pub struct L2Op {
    pub opid: u16,
    pub dst: SlotId,
    pub src: [SlotId; 2],
    pub accel: AtomicU32,
}

impl L2Op {
    /// A fresh interpreted op (no acceleration).
    pub fn new(opid: u16, dst: SlotId, src: [SlotId; 2]) -> Self {
        Self {
            opid,
            dst,
            src,
            accel: AtomicU32::new(ACCEL_INTERPRET),
        }
    }
}

/// The runtime linear instruction stream consumed by L3 (§9.3).
pub struct OpStream {
    pub ops: Vec<L2Op>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::Ordering;

    #[test]
    fn node_construction() {
        let n = Node {
            id: ValId {
                scope_level: 0,
                offset: 3,
            },
            op: Op::Add,
            ins: SmallVec::from_slice(&[
                ValId {
                    scope_level: 0,
                    offset: 1,
                },
                ValId {
                    scope_level: 0,
                    offset: 2,
                },
            ]),
            ty: Tag::I32,
            effect: Effect::Pure,
        };
        assert_eq!(n.ins.len(), 2);
        assert_eq!(n.ty, Tag::I32);
        assert!(matches!(n.op, Op::Add));
    }

    #[test]
    fn accel_seam_defaults_to_interpret() {
        let op = L2Op::new(5, SlotId(0), [SlotId(1), SlotId(2)]);
        assert_eq!(op.accel.load(Ordering::Acquire), ACCEL_INTERPRET);
        // Fragment replacement is a single CAS from the interpret indicator to a blob id.
        assert!(op
            .accel
            .compare_exchange(
                ACCEL_INTERPRET,
                ACCEL_BLOB_BASE,
                Ordering::AcqRel,
                Ordering::Acquire
            )
            .is_ok());
        assert_eq!(op.accel.load(Ordering::Acquire), ACCEL_BLOB_BASE);
    }
}
