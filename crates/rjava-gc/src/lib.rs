//! rjava-gc — object heap and reclamation (RJVM-SPEC-001 §5, §6). Increment 3 introduces the
//! object [`Heap`]: a lifecycle-managed store mapping [`RefIndex`] to object storage (§4.3). Every
//! object carries its [`EscapeState`] (§5) and a vector of 128-bit field values; guest code reaches
//! it only through the bounds-checked registry, never a host address (§4.3 memory-safety
//! corollary). Allocation can fail — surfaced as `None` for the allocation-failure-into-slot
//! protocol (§22.1) — and S1 objects are reclaimed deterministically via [`Heap::free`] (RAII,
//! §5.1). The rc fast path (S2/S3) and the SATB concurrent cycle collector arrive in increments
//! 5/9; the seams (escape state, control-block placement) are already present in `rjava-core`.

use rjava_core::{ClassId, EscapeState, RefIndex, Val128};

/// A heap object: its defining class, escape state, 128-bit field slots, and — once shared (S2+) —
/// its reference count (§4, §5).
#[derive(Debug, Clone)]
pub struct Object {
    pub class: ClassId,
    pub escape: EscapeState,
    pub fields: Vec<Val128>,
    /// Strong references naming this object. Meaningful from S2 on (§5.2): a **non-atomic** count,
    /// because an S2 object is only ever reachable from one virtual thread — no control block, no
    /// back-set, no lock. S1 objects have no count at all (RAII, §5.1); the CASP-atomic control
    /// block for genuinely cross-thread S3 objects arrives in increment 9 (§5.3, §4.4).
    ///
    /// Signed so that a stray decrement is caught by an assertion rather than wrapping.
    rc: i64,
}

/// The object heap: an indirection table from [`RefIndex`] to object storage (§4.3). A free list
/// recycles indices freed by RAII (§5.1); an allocation watermark models memory pressure so
/// allocation can fail (§22.1).
pub struct Heap {
    slots: Vec<Option<Object>>,
    free: Vec<u64>,
    live: usize,
    limit: usize,
}

impl Heap {
    /// A heap that can hold up to `limit` simultaneously-live objects before allocation fails.
    pub fn with_limit(limit: usize) -> Self {
        Heap {
            slots: Vec::new(),
            free: Vec::new(),
            live: 0,
            limit,
        }
    }

    /// A heap with a generous default limit.
    pub fn new() -> Self {
        Heap::with_limit(1 << 24)
    }

    /// Allocate a fresh object with the given (type-appropriate zero) field values. Returns its
    /// [`RefIndex`], or `None` when the live-object watermark is reached (allocation failure,
    /// §22.1). The caller supplies typed zeros (int→0, long→0L, float→0.0, ref→null) per JVMS
    /// default-value semantics.
    pub fn alloc(
        &mut self,
        class: ClassId,
        escape: EscapeState,
        fields: Vec<Val128>,
    ) -> Option<RefIndex> {
        if self.live >= self.limit {
            return None;
        }
        let obj = Object {
            class,
            escape,
            fields,
            // A fresh object is named by nothing yet; the write that stores it into a slot or field
            // is what raises the count (§5.5 — deltas land in program order, never eagerly).
            rc: 0,
        };
        let idx = if let Some(i) = self.free.pop() {
            self.slots[i as usize] = Some(obj);
            i
        } else {
            let i = self.slots.len() as u64;
            self.slots.push(Some(obj));
            i
        };
        self.live += 1;
        Some(RefIndex(idx))
    }

    /// Borrow an object.
    pub fn get(&self, r: RefIndex) -> Option<&Object> {
        self.slots.get(r.0 as usize).and_then(Option::as_ref)
    }

    /// Read field `i` of object `r`.
    pub fn get_field(&self, r: RefIndex, i: usize) -> Option<Val128> {
        self.get(r).and_then(|o| o.fields.get(i).copied())
    }

    /// Write field `i` of object `r`; returns `false` if the reference or index is invalid.
    pub fn set_field(&mut self, r: RefIndex, i: usize, v: Val128) -> bool {
        match self.slots.get_mut(r.0 as usize).and_then(Option::as_mut) {
            Some(o) => match o.fields.get_mut(i) {
                Some(slot) => {
                    *slot = v;
                    true
                }
                None => false,
            },
            None => false,
        }
    }

    /// Reclaim a **scope-exclusive (S1)** object at scope exit (RAII, §5.1); its index returns to
    /// the free list. Returns `false` — reclaiming nothing — for an S2/S3 object or an invalid
    /// reference: a shared object must be released through its reference-count lifecycle (§5.2,
    /// §5.3, increment 5), because recycling its index while `ptr` values still name it would let
    /// them address a later, unrelated object.
    pub fn free(&mut self, r: RefIndex) -> bool {
        let slot = match self.slots.get_mut(r.0 as usize) {
            Some(s) => s,
            None => return false,
        };
        match slot.as_ref().map(|o| o.escape) {
            Some(EscapeState::S1) => {
                *slot = None;
                self.free.push(r.0);
                self.live -= 1;
                true
            }
            _ => false,
        }
    }

    /// Number of currently-live objects.
    pub fn live(&self) -> usize {
        self.live
    }

    /// This object's escape state, if it is live.
    pub fn escape_of(&self, r: RefIndex) -> Option<EscapeState> {
        self.get(r).map(|o| o.escape)
    }

    /// This object's strong reference count (S2+); `None` if the reference is not live.
    pub fn rc(&self, r: RefIndex) -> Option<i64> {
        self.get(r).map(|o| o.rc)
    }

    /// Promote an object's escape state (§5.4). Promotion is **monotonic** — S1 → S2 → S3, never
    /// demoted — because an object that has been observed more widely can never become exclusive
    /// again. Promoting S1 → S2 is what installs the non-atomic reference count (§5.2); it happens
    /// when the object is aliased, stored into a longer-lived object, or returned (§9.4).
    /// Returns `false` if the reference is not live.
    pub fn promote(&mut self, r: RefIndex, to: EscapeState) -> bool {
        match self.slots.get_mut(r.0 as usize).and_then(Option::as_mut) {
            Some(o) => {
                o.escape.promote(to);
                true
            }
            None => false,
        }
    }

    /// Apply a reference-count delta to a shared (S2+) object, reclaiming it when the count reaches
    /// zero. Returns the new count, or `None` if the reference is not live.
    ///
    /// Callers MUST apply deltas at **diff-landing time, in program order** — never eagerly during
    /// speculation (§5.5). Counts are metadata: they never merge otherwise-independent chains.
    ///
    /// S1 objects carry no count and are reclaimed by RAII instead ([`Heap::free`], §5.1), so a
    /// delta against one is ignored.
    pub fn add_rc(&mut self, r: RefIndex, delta: i64) -> Option<i64> {
        let slot = self.slots.get_mut(r.0 as usize)?;
        let obj = slot.as_mut()?;
        if obj.escape == EscapeState::S1 {
            return Some(0); // scope-exclusive: RAII, not reference counting
        }
        obj.rc += delta;
        debug_assert!(
            obj.rc >= 0,
            "reference count underflowed for {r:?} — a release without a matching acquire (§5.5)"
        );
        let now = obj.rc;
        if now <= 0 {
            // The last reference went away: reclaim immediately (the acyclic fast path, §6.2).
            // Strong cycles are collected by the concurrent cycle collector in increment 9 (§6.3).
            *slot = None;
            self.free.push(r.0);
            self.live -= 1;
        }
        Some(now)
    }
}

impl Default for Heap {
    fn default() -> Self {
        Heap::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_fields_get_set() {
        let mut h = Heap::new();
        let r = h
            .alloc(ClassId(7), EscapeState::S1, vec![Val128::null(); 3])
            .unwrap();
        // Fields start as null.
        assert_eq!(h.get_field(r, 0).unwrap(), Val128::null());
        assert!(h.set_field(r, 1, Val128::from_i32(42)));
        assert_eq!(h.get_field(r, 1).unwrap().as_i32(), 42);
        assert_eq!(h.get(r).unwrap().class, ClassId(7));
        assert_eq!(h.get(r).unwrap().escape, EscapeState::S1);
        // Out-of-range field / reference are rejected, not panics.
        assert!(!h.set_field(r, 9, Val128::null()));
        assert!(h.get_field(RefIndex(999), 0).is_none());
    }

    #[test]
    fn free_recycles_index_and_tracks_live() {
        let mut h = Heap::new();
        let a = h
            .alloc(ClassId(0), EscapeState::S1, vec![Val128::null(); 1])
            .unwrap();
        let _b = h
            .alloc(ClassId(0), EscapeState::S1, vec![Val128::null(); 1])
            .unwrap();
        assert_eq!(h.live(), 2);
        assert!(h.free(a));
        assert_eq!(h.live(), 1);
        assert!(h.get(a).is_none());
        // The freed index is reused by the next allocation.
        let c = h
            .alloc(ClassId(0), EscapeState::S1, vec![Val128::null(); 1])
            .unwrap();
        assert_eq!(c, a);
        assert_eq!(h.live(), 2);
    }

    #[test]
    fn free_refuses_shared_objects_and_bad_references() {
        // Regression for a review finding: recycling an S2/S3 index while `ptr` values still name
        // it would let them address a later, unrelated object. Shared objects are released through
        // their reference-count lifecycle instead (§5.2, §5.3).
        let mut h = Heap::new();
        let s2 = h.alloc(ClassId(0), EscapeState::S2, Vec::new()).unwrap();
        let s3 = h.alloc(ClassId(0), EscapeState::S3, Vec::new()).unwrap();
        assert!(!h.free(s2), "S2 must not be freed by the RAII path");
        assert!(!h.free(s3), "S3 must not be freed by the RAII path");
        assert!(h.get(s2).is_some());
        assert!(h.get(s3).is_some());
        assert_eq!(h.live(), 2);
        // A dangling or already-freed reference is a no-op, never a panic or a live-count underflow.
        assert!(!h.free(RefIndex(9999)));
        let s1 = h.alloc(ClassId(0), EscapeState::S1, Vec::new()).unwrap();
        assert!(h.free(s1));
        assert!(!h.free(s1), "double free is refused");
        assert_eq!(h.live(), 2);
    }

    #[test]
    fn promotion_installs_a_count_and_is_monotonic() {
        let mut h = Heap::new();
        let r = h.alloc(ClassId(0), EscapeState::S1, Vec::new()).unwrap();
        assert_eq!(h.escape_of(r), Some(EscapeState::S1));
        // S1 carries no count: a delta against it is ignored (RAII governs it, §5.1).
        assert_eq!(h.add_rc(r, 1), Some(0));
        assert_eq!(h.rc(r), Some(0));

        assert!(h.promote(r, EscapeState::S2));
        assert_eq!(h.escape_of(r), Some(EscapeState::S2));
        // Monotonic: promoting "back" to S1 does nothing (§5.4).
        assert!(h.promote(r, EscapeState::S1));
        assert_eq!(h.escape_of(r), Some(EscapeState::S2));
        assert!(h.promote(r, EscapeState::S3));
        assert_eq!(h.escape_of(r), Some(EscapeState::S3));
    }

    #[test]
    fn refcount_reaching_zero_reclaims() {
        let mut h = Heap::new();
        let r = h.alloc(ClassId(0), EscapeState::S2, Vec::new()).unwrap();
        assert_eq!(h.live(), 1);
        assert_eq!(h.rc(r), Some(0));
        // Two references acquired, e.g. a local slot and an object field.
        assert_eq!(h.add_rc(r, 1), Some(1));
        assert_eq!(h.add_rc(r, 1), Some(2));
        assert_eq!(h.live(), 1);
        // Releasing one leaves it live...
        assert_eq!(h.add_rc(r, -1), Some(1));
        assert!(h.get(r).is_some());
        // ...releasing the last reclaims immediately (acyclic fast path, §6.2).
        assert_eq!(h.add_rc(r, -1), Some(0));
        assert!(h.get(r).is_none());
        assert_eq!(h.live(), 0);
        // The index is recycled, and deltas against a dead reference are refused.
        assert_eq!(h.add_rc(r, 1), None);
        let again = h.alloc(ClassId(1), EscapeState::S2, Vec::new()).unwrap();
        assert_eq!(again, r);
        assert_eq!(h.rc(again), Some(0), "a recycled slot starts fresh");
    }

    #[test]
    fn allocation_fails_at_the_watermark() {
        // §22.1: a failed allocation returns None (surfaced later as OutOfMemoryError).
        let mut h = Heap::with_limit(2);
        assert!(h.alloc(ClassId(0), EscapeState::S1, Vec::new()).is_some());
        assert!(h.alloc(ClassId(0), EscapeState::S1, Vec::new()).is_some());
        assert!(h.alloc(ClassId(0), EscapeState::S1, Vec::new()).is_none());
    }
}
