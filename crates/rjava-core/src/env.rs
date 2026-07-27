//! Execution environment: per-scope slot storage (RJVM-SPEC-001 §9.5) accessed through the
//! `read_slot`/`write_slot` indirection that increment 4 interposes copy-on-write / fork upon
//! (§10.4). Increment 1 uses it directly (no fork, no speculation).
//!
//! Exposing the read/write indirection on day one is deliberate: it is the seam that lets the
//! speculative fork/COW machinery be *added* later without rewriting the interpreter's slot
//! accesses — the anti-"code-layering-bug" invariant.

use crate::ids::SlotId;
use crate::value::Val128;

/// Maximum simultaneous variables per scope; the `0..1024` id ring (§9.5). A scope needing more
/// MUST be split into a child scope with `super` labels (handled by the IR builder).
pub const MAX_SLOTS: usize = 1024;

/// Logical Java frame identity. Maintained INDEPENDENTLY of execution order so that
/// `fillInStackTrace` reports the logical call chain even under out-of-order / inlined execution
/// (§20.7). Seeded when a (sub-)scope is entered; increment 2 wires it into call frames.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LogicalFrame(pub u32);

/// A single scope's slot storage.
pub struct Env {
    slots: Vec<Val128>,
    frame: LogicalFrame,
}

impl Env {
    /// A fresh environment with `n_slots` slots (`<= MAX_SLOTS`) initialised to `null`.
    pub fn new(n_slots: usize, frame: LogicalFrame) -> Self {
        debug_assert!(
            n_slots <= MAX_SLOTS,
            "scope exceeds 1024 slots; must be split (§9.5)"
        );
        Env {
            slots: vec![Val128::null(); n_slots],
            frame,
        }
    }

    /// Read a slot. THE indirection seam: increment 4 makes this consult a COW fork snapshot.
    #[inline]
    pub fn read_slot(&self, s: SlotId) -> Val128 {
        self.slots[s.index()]
    }

    /// Write a slot. THE indirection seam: increment 4 makes this a copy-on-write into the fork.
    #[inline]
    pub fn write_slot(&mut self, s: SlotId, v: Val128) {
        self.slots[s.index()] = v;
    }

    /// The logical Java frame this scope belongs to (§20.7).
    #[inline]
    pub fn frame(&self) -> LogicalFrame {
        self.frame
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.slots.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Tag;

    #[test]
    fn slots_default_null_then_roundtrip() {
        let mut e = Env::new(4, LogicalFrame(0));
        assert_eq!(e.len(), 4);
        assert_eq!(e.read_slot(SlotId(0)).tag(), Tag::Null);
        e.write_slot(SlotId(2), Val128::from_i32(42));
        assert_eq!(e.read_slot(SlotId(2)).as_i32(), 42);
        assert_eq!(e.read_slot(SlotId(0)).tag(), Tag::Null);
        assert_eq!(e.frame(), LogicalFrame(0));
    }
}
