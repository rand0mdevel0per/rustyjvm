//! Locking facilities (RJVM-SPEC-001 §12). TWO DISTINCT facilities that MUST NOT be mixed (§12.1):
//! the compact field read/write lock (for compound field transactions like `x += 1`) and the
//! reentrant monitor (for `synchronized`). `volatile` uses neither — it is lock-free ordered
//! (§11.6).
//!
//! Short critical sections spin; `park_or_spin` degrades to parking (yielding the carrier) once
//! `tmgr` exists (increment 9). M1 spins.

use core::hint::spin_loop;
use core::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use portable_atomic::{AtomicU16, AtomicU32};

// ---- compact u16 read/write lock (§12.2) ----
// bit 15..2 (14 bits): reader count (max 16383) | bit 1: write-waiting | bit 0: write-held

const W_HELD_16: u16 = 0b01;
const W_WAIT_16: u16 = 0b10;
const R_UNIT_16: u16 = 0b100;

/// Compact 16-bit read/write lock for cold S2/S3 field transactions (§12.2). Write-waiting blocks
/// new readers to prevent writer starvation. Non-reentrant.
pub struct U16Lock(AtomicU16);

impl U16Lock {
    pub const fn new() -> Self {
        Self(AtomicU16::new(0))
    }

    pub fn read(&self) {
        loop {
            let s = self.0.load(Acquire);
            if s & (W_HELD_16 | W_WAIT_16) == 0 {
                if self
                    .0
                    .compare_exchange_weak(s, s + R_UNIT_16, Acquire, Relaxed)
                    .is_ok()
                {
                    return;
                }
            } else {
                spin_loop();
            }
        }
    }

    pub fn read_unlock(&self) {
        self.0.fetch_sub(R_UNIT_16, Release);
    }

    pub fn write(&self) {
        // 1. claim write-waiting to block new readers (starvation-free).
        loop {
            let s = self.0.load(Acquire);
            if s & W_WAIT_16 == 0 {
                if self
                    .0
                    .compare_exchange_weak(s, s | W_WAIT_16, Acquire, Relaxed)
                    .is_ok()
                {
                    break;
                }
            } else {
                spin_loop();
            }
        }
        // 2. wait for readers to drain, then claim write-held (clearing write-waiting).
        loop {
            let s = self.0.load(Acquire);
            if s >> 2 == 0 && s & W_HELD_16 == 0 {
                if self
                    .0
                    .compare_exchange_weak(s, W_HELD_16, AcqRel, Relaxed)
                    .is_ok()
                {
                    return;
                }
            } else {
                spin_loop();
            }
        }
    }

    pub fn write_unlock(&self) {
        self.0.store(0, Release);
    }
}

impl Default for U16Lock {
    fn default() -> Self {
        Self::new()
    }
}

// ---- u32 read/write lock for hot S3 objects (§12.7): 30-bit reader count ----

const W_HELD_32: u32 = 0b01;
const W_WAIT_32: u32 = 0b10;
const R_UNIT_32: u32 = 0b100;

/// Wider lock for hot S3 objects with potentially thousands of concurrent readers (§12.7).
/// A cold object keeps its [`U16Lock`]; the upgrade to `U32Lock` happens at the S2->S3 promotion
/// point for hot objects, costing 2 extra bytes only where needed.
pub struct U32Lock(AtomicU32);

impl U32Lock {
    pub const fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    pub fn read(&self) {
        loop {
            let s = self.0.load(Acquire);
            if s & (W_HELD_32 | W_WAIT_32) == 0 {
                if self
                    .0
                    .compare_exchange_weak(s, s + R_UNIT_32, Acquire, Relaxed)
                    .is_ok()
                {
                    return;
                }
            } else {
                spin_loop();
            }
        }
    }

    pub fn read_unlock(&self) {
        self.0.fetch_sub(R_UNIT_32, Release);
    }

    pub fn write(&self) {
        loop {
            let s = self.0.load(Acquire);
            if s & W_WAIT_32 == 0 {
                if self
                    .0
                    .compare_exchange_weak(s, s | W_WAIT_32, Acquire, Relaxed)
                    .is_ok()
                {
                    break;
                }
            } else {
                spin_loop();
            }
        }
        loop {
            let s = self.0.load(Acquire);
            if s >> 2 == 0 && s & W_HELD_32 == 0 {
                if self
                    .0
                    .compare_exchange_weak(s, W_HELD_32, AcqRel, Relaxed)
                    .is_ok()
                {
                    return;
                }
            } else {
                spin_loop();
            }
        }
    }

    pub fn write_unlock(&self) {
        self.0.store(0, Release);
    }
}

impl Default for U32Lock {
    fn default() -> Self {
        Self::new()
    }
}

// ---- reentrant monitor for `synchronized` (§12.1, §11.6) ----

const NO_OWNER: u32 = u32::MAX;

/// Reentrant monitor for `synchronized` (§12.1). `monitorenter` = Acquire, `monitorexit` =
/// Release; reentrant via (owner, count). This is NOT the field lock and MUST NOT be conflated
/// with it. `wait`/`notify` + fair queuing arrive with `tmgr` in increment 9; M1 spins.
pub struct Monitor {
    owner: AtomicU32, // a VtId's bits, or NO_OWNER
    count: AtomicU32, // reentry depth; only mutated by the current owner
}

impl Monitor {
    pub const fn new() -> Self {
        Self {
            owner: AtomicU32::new(NO_OWNER),
            count: AtomicU32::new(0),
        }
    }

    /// Enter for virtual thread `vt`. Reentrant: re-entry by the owner just bumps the count.
    pub fn enter(&self, vt: u32) {
        if self.owner.load(Acquire) == vt {
            self.count.fetch_add(1, Relaxed);
            return;
        }
        loop {
            if self
                .owner
                .compare_exchange(NO_OWNER, vt, Acquire, Relaxed)
                .is_ok()
            {
                self.count.store(1, Relaxed);
                return;
            }
            spin_loop();
        }
    }

    /// Exit for virtual thread `vt`. Releases the monitor when the reentry count reaches zero.
    pub fn exit(&self, vt: u32) {
        debug_assert_eq!(
            self.owner.load(Relaxed),
            vt,
            "monitorexit by non-owner (§12.1)"
        );
        if self.count.fetch_sub(1, Relaxed) == 1 {
            self.owner.store(NO_OWNER, Release);
        }
    }

    /// The current owner vt, or `None` if unlocked.
    pub fn owner(&self) -> Option<u32> {
        match self.owner.load(Acquire) {
            NO_OWNER => None,
            o => Some(o),
        }
    }
}

impl Default for Monitor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::UnsafeCell;
    use std::sync::Arc;
    use std::thread;

    /// The u16 write lock must give mutual exclusion: 8 threads each increment a shared counter
    /// 1000 times under the write lock; the total must be exact.
    #[test]
    fn u16_write_lock_mutual_exclusion() {
        struct Shared {
            lock: U16Lock,
            val: UnsafeCell<u64>,
        }
        // SAFETY: `val` is only ever touched while the write lock is held (single writer at a time).
        unsafe impl Sync for Shared {}

        let s = Arc::new(Shared {
            lock: U16Lock::new(),
            val: UnsafeCell::new(0),
        });
        let mut handles = Vec::new();
        for _ in 0..8 {
            let s = s.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    s.lock.write();
                    // SAFETY: write lock held -> exclusive access.
                    unsafe {
                        *s.val.get() += 1;
                    }
                    s.lock.write_unlock();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(unsafe { *s.val.get() }, 8000);
    }

    #[test]
    fn monitor_is_reentrant() {
        let m = Monitor::new();
        m.enter(7);
        m.enter(7); // reentry by owner
        assert_eq!(m.owner(), Some(7));
        m.exit(7);
        assert_eq!(m.owner(), Some(7)); // still held (count was 2)
        m.exit(7);
        assert_eq!(m.owner(), None);
    }

    #[test]
    fn u16_read_then_write() {
        let l = U16Lock::new();
        l.read();
        l.read();
        l.read_unlock();
        l.read_unlock();
        l.write();
        l.write_unlock();
    }
}
