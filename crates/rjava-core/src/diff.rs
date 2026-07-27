//! Speculative-execution unit and escape state (RJVM-SPEC-001 §5, §9.6, §10).
//!
//! The diff/dep engine forks environments (COW), executes chains, and lands their diffs in
//! program order (§10.4–10.5). In M1 the machinery is present and structurally exercised, but
//! speculation is load-gated OFF (rate = 0): every chain runs concretely and lands in program
//! order (increment 4). Value prediction via `deps` activates in M2.

use crate::ids::{SlotId, VtId};
use crate::value::Val128;

/// The escape state of a heap object (§5). Governs which concurrency / reclamation machinery
/// applies. Promotion is monotonic S1 -> S2 -> S3, never demoted (§5.4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum EscapeState {
    /// Scope-exclusive: handle, RAII, genuinely zero GC cost (§5.1). Maximise this (P-2).
    S1 = 0,
    /// Thread-local shared: non-atomic refcount, intra-vt only (§5.2).
    S2 = 1,
    /// Cross-thread shared: control block, CASP atomics, lock, cycle collector (§5.3).
    S3 = 2,
}

impl EscapeState {
    /// Monotonic promotion (§5.4): moves toward the more general state; never demotes. When
    /// non-escape cannot be proven, classification MUST err toward the more general state (§9.4).
    pub fn promote(&mut self, to: EscapeState) {
        if (to as u8) > (*self as u8) {
            *self = to;
        }
    }
}

/// A copy-on-write snapshot of environment slots a diff chain executes against (§10.4). M1: a
/// plain owned slot vector. Increment 4 makes sharing structural (COW); a later pass may switch to
/// a persistent representation. S3 objects are NEVER forked into a snapshot (§10.4) — they stay in
/// the shared heap behind the lock.
#[derive(Clone, Default)]
pub struct EnvSnapshot {
    pub slots: Vec<Val128>,
}

/// A speculative execution unit over an environment snapshot (§9.6).
pub struct DiffNode {
    /// Owning virtual thread; trees of different tid never merge (§10.5).
    pub tid: VtId,
    /// Inherits the predecessor's result (`init = predecessor(init + diff)`, §10.4).
    pub init: EnvSnapshot,
    /// Writes produced by this node (applied at program-order landing, §10.5).
    pub diff: Vec<(SlotId, Val128)>,
    /// Speculative assumptions (value-prediction deps); empty while speculation is gated off.
    pub deps: Vec<(SlotId, Val128)>,
    /// Program-order landing key (§10.5).
    pub po: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promotion_is_monotonic() {
        let mut s = EscapeState::S1;
        s.promote(EscapeState::S2);
        assert_eq!(s, EscapeState::S2);
        s.promote(EscapeState::S1); // never demote
        assert_eq!(s, EscapeState::S2);
        s.promote(EscapeState::S3);
        assert_eq!(s, EscapeState::S3);
        s.promote(EscapeState::S1); // still S3
        assert_eq!(s, EscapeState::S3);
    }
}
