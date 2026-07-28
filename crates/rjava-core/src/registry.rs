//! Object registry and lifecycle control blocks (RJVM-SPEC-001 §4.3–4.4).
//!
//! Guest references are indices into a bounds-checked store, never host addresses — so no guest
//! input, however malicious, can make the host dereference arbitrary memory (§4.3 memory-safety
//! corollary). Mutable per-object lifecycle metadata (reference counts, validity) lives in a
//! SEPARATE control block, never inside the copied 128-bit pointer value, so pointer copies never
//! tear on metadata mutation (§4.4, §12.6).

use core::marker::PhantomData;
use core::sync::atomic::Ordering;
use portable_atomic::{AtomicBool, AtomicU128};

use crate::ids::RegistryKey;

/// A vector indexed by a newtype key `K` (§8.4). Bounds-checked indirection; the substrate for
/// both the class registry and the object registry.
pub struct RegistryVec<K, V> {
    items: Vec<V>,
    _k: PhantomData<K>,
}

impl<K: RegistryKey, V> RegistryVec<K, V> {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            _k: PhantomData,
        }
    }
    pub fn with_capacity(n: usize) -> Self {
        Self {
            items: Vec::with_capacity(n),
            _k: PhantomData,
        }
    }
    /// Append `v`, returning its freshly assigned key.
    pub fn push(&mut self, v: V) -> K {
        let k = K::from_index(self.items.len());
        self.items.push(v);
        k
    }
    pub fn get(&self, k: K) -> Option<&V> {
        self.items.get(k.index())
    }
    pub fn get_mut(&mut self, k: K) -> Option<&mut V> {
        self.items.get_mut(k.index())
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

impl<K: RegistryKey, V> Default for RegistryVec<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

/// A snapshot of an object's strong/weak reference counts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RefCounts {
    pub strong: u64,
    pub weak: u64,
}

impl RefCounts {
    #[inline]
    const fn pack(self) -> u128 {
        ((self.strong as u128) << 64) | (self.weak as u128)
    }
    #[inline]
    const fn unpack(bits: u128) -> RefCounts {
        RefCounts {
            strong: (bits >> 64) as u64,
            weak: bits as u64,
        }
    }
}

/// Per-object control block for S3 (cross-thread shared) objects (§4.4, §5.3).
///
/// The (strong, weak) pair is a single 128-bit atomic updated with CASP via `portable-atomic`,
/// which feature-detects LSE `CASP` versus an `LDXP`/`STXP` LL-SC fallback on ARM64 (§12.6).
/// Keeping the mutable counts here — out of the copied pointer value — is what makes the pointer
/// value an immutable, tear-free datum: concurrent 128-bit copies never observe a half-updated
/// count (§4.4).
pub struct ControlBlock {
    counts: AtomicU128,
    valid: AtomicBool,
}

impl ControlBlock {
    /// A block with `strong` strong references, 0 weak, valid.
    pub fn new(strong: u64) -> Self {
        Self {
            counts: AtomicU128::new(RefCounts { strong, weak: 0 }.pack()),
            valid: AtomicBool::new(true),
        }
    }

    /// Current (strong, weak) counts.
    pub fn counts(&self) -> RefCounts {
        RefCounts::unpack(self.counts.load(Ordering::Acquire))
    }

    /// Whether the object is still valid (not swept / not yet destroyed) (§6.3).
    pub fn is_valid(&self) -> bool {
        self.valid.load(Ordering::Acquire)
    }

    /// Invalidate (cycle-collector sweep, or `strong == 0`). Returns the previous validity.
    pub fn invalidate(&self) -> bool {
        self.valid.swap(false, Ordering::AcqRel)
    }

    /// Atomically add `delta` to the strong count; returns the new strong count.
    pub fn add_strong(&self, delta: i64) -> u64 {
        let mut cur = self.counts.load(Ordering::Acquire);
        loop {
            let mut c = RefCounts::unpack(cur);
            c.strong = (c.strong as i128 + delta as i128) as u64;
            match self.counts.compare_exchange_weak(
                cur,
                c.pack(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return c.strong,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Atomically add `delta` to the weak count; returns the new weak count.
    pub fn add_weak(&self, delta: i64) -> u64 {
        let mut cur = self.counts.load(Ordering::Acquire);
        loop {
            let mut c = RefCounts::unpack(cur);
            c.weak = (c.weak as i128 + delta as i128) as u64;
            match self.counts.compare_exchange_weak(
                cur,
                c.pack(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return c.weak,
                Err(actual) => cur = actual,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ClassId;

    #[test]
    fn registry_vec_push_get() {
        let mut r: RegistryVec<ClassId, u32> = RegistryVec::new();
        let a = r.push(10);
        let b = r.push(20);
        assert_eq!(a, ClassId(0));
        assert_eq!(b, ClassId(1));
        assert_eq!(r.get(a), Some(&10));
        assert_eq!(r.get(b), Some(&20));
        assert_eq!(r.len(), 2);
        assert!(r.get(ClassId(9)).is_none());
    }

    #[test]
    fn control_block_counts_and_validity() {
        let cb = ControlBlock::new(1);
        assert_eq!(cb.counts(), RefCounts { strong: 1, weak: 0 });
        assert_eq!(cb.add_strong(1), 2);
        assert_eq!(cb.add_weak(3), 3);
        assert_eq!(cb.add_strong(-1), 1);
        assert_eq!(cb.counts(), RefCounts { strong: 1, weak: 3 });
        assert!(cb.is_valid());
        assert!(cb.invalidate());
        assert!(!cb.is_valid());
    }

    /// Exercises the 128-bit atomic CAS path (§12.6) under contention: 8 threads each add 1000 to
    /// both counts. The final packed pair must be exact — proving no torn read/update.
    #[test]
    fn control_block_128bit_cas_under_contention() {
        use std::sync::Arc;
        use std::thread;
        let cb = Arc::new(ControlBlock::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let c = cb.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    c.add_strong(1);
                    c.add_weak(1);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            cb.counts(),
            RefCounts {
                strong: 8000,
                weak: 8000
            }
        );
    }

    /// On tier-1 targets (x86-64 `cmpxchg16b`; AArch64 `LDXP`/`STXP` or LSE `CASP`) the
    /// control-block's 128-bit atomics MUST be lock-free (§12.6); `portable-atomic`'s `fallback`
    /// keeps them merely correct elsewhere. Asserting it here makes "we are on the lock-free path"
    /// observable at test time, so a silent regression to the lock path is caught.
    #[test]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn control_block_128bit_atomics_are_lock_free() {
        assert!(
            portable_atomic::AtomicU128::is_lock_free(),
            "128-bit control-block atomics are not lock-free on this target (§12.6)"
        );
    }
}
