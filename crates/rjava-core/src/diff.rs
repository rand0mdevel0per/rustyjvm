//! Speculative-execution units, escape state, and fork bookkeeping (RJVM-SPEC-001 §5, §9.6, §10).
//!
//! The diff/dep engine forks environments (copy-on-write), executes chains, and lands their diffs
//! in program order (§10.4–10.5). In M1 speculation is load-gated OFF (rate 0): every chain runs
//! concretely and lands in program order, so the machinery is exercised structurally without value
//! prediction. `deps` activates in M2.

use crate::ids::{RefIndex, SlotId, VtId};
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

/// A copy-on-write snapshot of environment slots a diff chain executes against (§10.4). S3 objects
/// are NEVER forked into a snapshot (§10.4) — they stay in the shared heap behind the lock, so a
/// write in one vt cannot be hidden from another (the foundation of speculation correctness).
#[derive(Clone, Default, Debug)]
pub struct EnvSnapshot {
    pub slots: Vec<Val128>,
}

/// Identifies one instance field of one object: the granularity at which chains genuinely conflict.
pub type FieldKey = (RefIndex, u16);

/// A speculative execution unit over an environment snapshot (§9.6).
#[derive(Default)]
pub struct DiffNode {
    /// Owning virtual thread; trees of different tid never merge (§10.5).
    pub tid: VtId,
    /// Inherits the predecessor's result (`init = predecessor(init + diff)`, §10.4).
    pub init: EnvSnapshot,
    /// Slot writes produced by this node, applied at program-order landing (§10.5).
    pub diff: Vec<(SlotId, Val128)>,
    /// Speculative assumptions (value-prediction deps); empty while speculation is gated off.
    pub deps: Vec<(SlotId, Val128)>,
    /// Heap fields this chain wrote — a genuine data dependency (§10.5).
    pub field_writes: Vec<FieldKey>,
    /// Heap fields this chain read — conflicts with another chain's write (§10.5).
    pub field_reads: Vec<FieldKey>,
    /// Reference-count adjustments. **Metadata, NOT a data dependency** (§5.5): these MUST NOT
    /// cause two otherwise-independent chains to merge, and MUST be applied serially in program
    /// order at landing time — never eagerly during speculation. Populated from increment 5.
    pub rc_deltas: Vec<(RefIndex, i32)>,
    /// Program-order landing key (§10.5).
    pub po: u32,
}

impl DiffNode {
    /// A chain owned by `tid`, inheriting `init`, landing at program-order key `po`.
    pub fn new(tid: VtId, init: EnvSnapshot, po: u32) -> Self {
        DiffNode {
            tid,
            init,
            po,
            ..Default::default()
        }
    }

    /// Record a heap-field read (a dependency, §10.5).
    pub fn record_field_read(&mut self, obj: RefIndex, field: u16) {
        self.field_reads.push((obj, field));
    }

    /// Record a heap-field write (a dependency, §10.5).
    pub fn record_field_write(&mut self, obj: RefIndex, field: u16) {
        self.field_writes.push((obj, field));
    }

    /// Record a reference-count adjustment — metadata only; applied in program order at landing,
    /// never eagerly, and never a merge trigger (§5.5).
    pub fn record_rc_delta(&mut self, obj: RefIndex, delta: i32) {
        self.rc_deltas.push((obj, delta));
    }
}

/// Whether two chains have a genuine **data** conflict and must therefore merge into one serial
/// chain (§10.5).
///
/// Conflicts are write/write or write/read on the same `(object, field)`. Reference-count
/// adjustments are deliberately excluded: treating rc as a dependency would serialise every chain
/// touching a hot object and destroy parallelism, and applying rc eagerly during speculation is a
/// silent memory-corruption source (§5.5). Chains of different tid never merge (§10.5) — they
/// coordinate through the S3 lock instead.
pub fn chains_conflict(a: &DiffNode, b: &DiffNode) -> bool {
    if a.tid != b.tid {
        return false; // different tid: never merged; S3 access is lock-mediated (§10.5, §11.4)
    }
    let hits = |writes: &[FieldKey], other_w: &[FieldKey], other_r: &[FieldKey]| {
        writes
            .iter()
            .any(|k| other_w.contains(k) || other_r.contains(k))
    };
    hits(&a.field_writes, &b.field_writes, &b.field_reads)
        || hits(&b.field_writes, &a.field_writes, &a.field_reads)
}

/// Live forked environment snapshots. The cycle collector MUST scan these as roots, or an object
/// reachable only from an in-flight speculative chain could be reclaimed (§6.3). Built here in
/// increment 4 so the increment-9 collector simply iterates it.
#[derive(Default)]
pub struct ForkRegistry {
    live: Vec<(u64, EnvSnapshot)>,
    next: u64,
}

impl ForkRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a live fork snapshot; the returned id releases it.
    pub fn register(&mut self, snap: EnvSnapshot) -> u64 {
        let id = self.next;
        self.next += 1;
        self.live.push((id, snap));
        id
    }

    /// Release a snapshot once its chain has landed or been discarded.
    pub fn release(&mut self, id: u64) {
        self.live.retain(|(i, _)| *i != id);
    }

    /// Every reference held by a live fork snapshot — GC roots (§6.3).
    pub fn roots(&self) -> impl Iterator<Item = RefIndex> + '_ {
        self.live.iter().flat_map(|(_, s)| {
            s.slots
                .iter()
                .filter(|v| v.tag().is_ref())
                .map(|v| v.ref_index())
        })
    }

    pub fn live_count(&self) -> usize {
        self.live.len()
    }
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

    fn chain(po: u32) -> DiffNode {
        DiffNode::new(VtId(0), EnvSnapshot::default(), po)
    }

    #[test]
    fn disjoint_field_chains_do_not_merge() {
        // §10.5: independent chains must stay parallel.
        let mut a = chain(0);
        a.record_field_write(RefIndex(1), 0);
        let mut b = chain(1);
        b.record_field_write(RefIndex(1), 1); // same object, DIFFERENT field
        assert!(!chains_conflict(&a, &b));

        let mut c = chain(2);
        c.record_field_write(RefIndex(2), 0); // different object, same field index
        assert!(!chains_conflict(&a, &c));
    }

    #[test]
    fn conflicting_field_chains_must_merge() {
        // write/write on the same (object, field)
        let mut a = chain(0);
        a.record_field_write(RefIndex(1), 0);
        let mut b = chain(1);
        b.record_field_write(RefIndex(1), 0);
        assert!(chains_conflict(&a, &b));

        // write/read on the same (object, field), in both directions
        let mut r = chain(2);
        r.record_field_read(RefIndex(1), 0);
        assert!(chains_conflict(&a, &r));
        assert!(chains_conflict(&r, &a));
    }

    #[test]
    fn rc_deltas_are_metadata_and_never_merge_chains() {
        // §5.5 (load-bearing): rc adjustments to the SAME object must not serialise chains that
        // write disjoint fields.
        let mut a = chain(0);
        a.record_field_write(RefIndex(9), 0);
        a.record_rc_delta(RefIndex(42), 1);
        let mut b = chain(1);
        b.record_field_write(RefIndex(9), 1);
        b.record_rc_delta(RefIndex(42), -1);
        assert!(!chains_conflict(&a, &b));
        // The deltas are still recorded, to be applied in program order at landing.
        assert_eq!(a.rc_deltas, vec![(RefIndex(42), 1)]);
        assert_eq!(b.rc_deltas, vec![(RefIndex(42), -1)]);
    }

    #[test]
    fn different_tid_chains_never_merge() {
        // §10.5: diff trees of different tid never merge; S3 access goes through the lock.
        let mut a = DiffNode::new(VtId(0), EnvSnapshot::default(), 0);
        a.record_field_write(RefIndex(1), 0);
        let mut b = DiffNode::new(VtId(1), EnvSnapshot::default(), 0);
        b.record_field_write(RefIndex(1), 0); // identical field, other vt
        assert!(!chains_conflict(&a, &b));
    }

    #[test]
    fn fork_snapshots_are_gc_roots() {
        // §6.3: an object reachable only from an in-flight fork must be enumerable as a root.
        // A snapshot never carries an S1 `handle` — a handle is move-only (§4.2), so anything a
        // second chain can reach has escaped and was promoted to a shared `ptr` first (§5.4).
        let mut reg = ForkRegistry::new();
        let snap = EnvSnapshot {
            slots: vec![
                Val128::from_i32(1),       // not a reference
                Val128::ptr(RefIndex(77)), // shared pointer
                Val128::ptr(RefIndex(88)),
                Val128::null(),
            ],
        };
        let id = reg.register(snap);
        let roots: Vec<_> = reg.roots().collect();
        assert_eq!(roots, vec![RefIndex(77), RefIndex(88)]);
        assert_eq!(reg.live_count(), 1);
        reg.release(id);
        assert_eq!(reg.roots().count(), 0);
        assert_eq!(reg.live_count(), 0);
    }
}
