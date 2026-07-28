//! rjava-gc — object heap and reclamation (RJVM-SPEC-001 §5, §6). Increment 3 introduces the
//! object [`Heap`]: a lifecycle-managed store mapping [`RefIndex`] to object storage (§4.3). Every
//! object carries its [`EscapeState`] (§5) and a vector of 128-bit field values; guest code reaches
//! it only through the bounds-checked registry, never a host address (§4.3 memory-safety
//! corollary). Allocation can fail — surfaced as `None` for the allocation-failure-into-slot
//! protocol (§22.1) — and S1 objects are reclaimed deterministically via [`Heap::free`] (RAII,
//! §5.1). The rc fast path (S2/S3) and the SATB concurrent cycle collector arrive in increments
//! 5/9; the seams (escape state, control-block placement) are already present in `rjava-core`.

use rjava_core::{ClassId, EscapeState, RefIndex, Val128};

/// A heap object: its defining class, escape state, and 128-bit field slots (§4, §5).
#[derive(Debug, Clone)]
pub struct Object {
    pub class: ClassId,
    pub escape: EscapeState,
    pub fields: Vec<Val128>,
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
        Heap { slots: Vec::new(), free: Vec::new(), live: 0, limit }
    }

    /// A heap with a generous default limit.
    pub fn new() -> Self {
        Heap::with_limit(1 << 24)
    }

    /// Allocate a fresh object with `n_fields` `null`-initialised fields. Returns its [`RefIndex`],
    /// or `None` when the live-object watermark is reached (allocation failure, §22.1).
    pub fn alloc(
        &mut self,
        class: ClassId,
        escape: EscapeState,
        n_fields: usize,
    ) -> Option<RefIndex> {
        if self.live >= self.limit {
            return None;
        }
        let obj = Object { class, escape, fields: vec![Val128::null(); n_fields] };
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

    /// Reclaim an object (S1 RAII / scope-exit drop, §5.1). Its index returns to the free list.
    pub fn free(&mut self, r: RefIndex) {
        if let Some(slot) = self.slots.get_mut(r.0 as usize) {
            if slot.take().is_some() {
                self.free.push(r.0);
                self.live -= 1;
            }
        }
    }

    /// Number of currently-live objects.
    pub fn live(&self) -> usize {
        self.live
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
        let r = h.alloc(ClassId(7), EscapeState::S1, 3).unwrap();
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
        let a = h.alloc(ClassId(0), EscapeState::S1, 1).unwrap();
        let _b = h.alloc(ClassId(0), EscapeState::S1, 1).unwrap();
        assert_eq!(h.live(), 2);
        h.free(a);
        assert_eq!(h.live(), 1);
        assert!(h.get(a).is_none());
        // The freed index is reused by the next allocation.
        let c = h.alloc(ClassId(0), EscapeState::S1, 1).unwrap();
        assert_eq!(c, a);
        assert_eq!(h.live(), 2);
    }

    #[test]
    fn allocation_fails_at_the_watermark() {
        // §22.1: a failed allocation returns None (surfaced later as OutOfMemoryError).
        let mut h = Heap::with_limit(2);
        assert!(h.alloc(ClassId(0), EscapeState::S1, 0).is_some());
        assert!(h.alloc(ClassId(0), EscapeState::S1, 0).is_some());
        assert!(h.alloc(ClassId(0), EscapeState::S1, 0).is_none());
    }
}
