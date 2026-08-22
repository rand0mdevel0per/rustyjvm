//! Execution environment: per-scope slot storage (RJVM-SPEC-001 §9.5) with the copy-on-write diff
//! seam that speculation is built on (§10.4–10.5).
//!
//! Writes do not mutate the landed state directly: they accumulate in a **pending diff** in program
//! order, and `land` applies them. Reads consult the pending diff first (newest write wins) and
//! fall back to the landed base — snapshot isolation within a chain (§10.4). Landing is
//! **program-order-incremental**, never batched at scope exit; that is what makes intra-vt
//! execution as-if-serial and, later, local exception handling correct (§10.5, §20.5).
//!
//! In M1 speculation is load-gated OFF (rate 0): every chain executes concretely and lands in
//! program order, so the machinery is exercised structurally without value prediction (§10.3).

use smallvec::SmallVec;

use crate::diff::EnvSnapshot;
use crate::ids::SlotId;
use crate::value::Val128;

/// Maximum simultaneous variables per scope; the `0..1024` id ring (§9.5). A scope needing more
/// MUST be split into a child scope with `super` labels (handled by the IR builder).
pub const MAX_SLOTS: usize = 1024;

/// Logical Java frame identity. Maintained INDEPENDENTLY of execution order so that
/// `fillInStackTrace` reports the logical call chain even under out-of-order / inlined execution
/// (§20.7). Seeded when a (sub-)scope is entered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LogicalFrame(pub u32);

/// A single scope's slot storage plus its undo journal.
///
/// Slots hold the *working* state so reads and writes are O(1). The journal records each write's
/// **previous** value in program order, which is what makes the diff semantics work: landing simply
/// forgets the journal, discarding replays it backwards, and a fork snapshot is reconstructed by
/// undoing it. (Recording pre-images rather than scanning a pending list keeps straight-line
/// execution linear instead of quadratic.)
pub struct Env {
    /// Working state: landed writes plus this chain's unlanded ones.
    slots: Vec<Val128>,
    /// Undo journal for unlanded writes: `(slot, value before the write)`, in program order.
    journal: SmallVec<[(u16, Val128); 8]>,
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
            journal: SmallVec::new(),
            frame,
        }
    }

    /// Read a slot. THE indirection seam (§10.4): this chain sees its own unlanded writes; another
    /// chain, which reads its own forked snapshot, does not (snapshot isolation).
    #[inline]
    pub fn read_slot(&self, s: SlotId) -> Val128 {
        self.slots[s.index()]
    }

    /// Write a slot. THE indirection seam (§10.4): the previous value is journalled first, so the
    /// write belongs to the unlanded diff until [`Env::land`] commits it.
    #[inline]
    pub fn write_slot(&mut self, s: SlotId, v: Val128) {
        self.journal.push((s.0, self.slots[s.index()]));
        self.slots[s.index()] = v;
    }

    /// Land the pending diff **in program order** (§10.5): the writes become permanent, so the undo
    /// journal is dropped. Called as program order advances — at chain (block) boundaries and
    /// before every `Effect::Extern` fence (§10.6) — never batched at scope exit.
    #[inline]
    pub fn land(&mut self) {
        self.journal.clear();
    }

    /// Discard unlanded writes by replaying the journal backwards (mis-speculation, or the
    /// `po > throw` tail of an abandoned path, §20.4–20.5). Landed state is never rolled back —
    /// the model is oneshot (§10.3).
    #[inline]
    pub fn discard_pending(&mut self) {
        while let Some((slot, old)) = self.journal.pop() {
            self.slots[slot as usize] = old;
        }
    }

    /// Number of unlanded writes.
    #[inline]
    pub fn pending_len(&self) -> usize {
        self.journal.len()
    }

    /// A copy-on-write snapshot of the **landed** state, for forking a parallel chain (§10.4) and
    /// as a GC root while the fork is live (§6.3).
    ///
    /// A snapshot must not carry an S1 `handle`: a handle is move-only and MUST NOT be duplicated
    /// (§4.2), so an object reachable by a second chain has by definition escaped and must first be
    /// promoted to S2/`ptr` (§5.4). Promotion is implemented in increment 5; until then the
    /// debug assertion below makes a violation loud rather than silent.
    pub fn snapshot(&self) -> EnvSnapshot {
        let mut slots = self.slots.clone();
        // Undo unlanded writes (reverse order) to recover the landed state.
        for &(slot, old) in self.journal.iter().rev() {
            slots[slot as usize] = old;
        }
        debug_assert!(
            !slots.iter().any(|v| v.tag() == crate::value::Tag::Handle),
            "an S1 handle must be promoted to S2 before it can be reached by a forked chain (§4.2, §5.4)"
        );
        EnvSnapshot { slots }
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

    #[test]
    fn writes_are_pending_until_landed_and_reads_see_them() {
        let mut e = Env::new(2, LogicalFrame(0));
        e.write_slot(SlotId(0), Val128::from_i32(7));
        // Unlanded, but visible to this chain (snapshot isolation, §10.4).
        assert_eq!(e.pending_len(), 1);
        assert_eq!(e.read_slot(SlotId(0)).as_i32(), 7);
        // A fork snapshot taken now sees only the LANDED state.
        assert_eq!(e.snapshot().slots[0].tag(), Tag::Null);
        e.land();
        assert_eq!(e.pending_len(), 0);
        assert_eq!(e.read_slot(SlotId(0)).as_i32(), 7);
        assert_eq!(e.snapshot().slots[0].as_i32(), 7);
    }

    #[test]
    fn newest_pending_write_wins_and_lands_in_program_order() {
        let mut e = Env::new(1, LogicalFrame(0));
        e.write_slot(SlotId(0), Val128::from_i32(1));
        e.write_slot(SlotId(0), Val128::from_i32(2));
        e.write_slot(SlotId(0), Val128::from_i32(3));
        assert_eq!(e.read_slot(SlotId(0)).as_i32(), 3);
        e.land(); // applied in order, so the last write is the final value
        assert_eq!(e.read_slot(SlotId(0)).as_i32(), 3);
    }

    #[test]
    fn discard_drops_only_unlanded_writes() {
        // §20.5: diffs with po < throw have landed and stay visible; po > throw are discarded.
        let mut e = Env::new(1, LogicalFrame(0));
        e.write_slot(SlotId(0), Val128::from_i32(5));
        e.land(); // po < throw
        e.write_slot(SlotId(0), Val128::from_i32(10)); // po > throw
        e.discard_pending();
        assert_eq!(e.read_slot(SlotId(0)).as_i32(), 5);
    }
}
