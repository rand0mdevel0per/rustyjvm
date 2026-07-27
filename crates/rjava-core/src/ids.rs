//! Newtype identifiers used across the engine (RJVM-SPEC-001 §8.4, §9). Cheap, pervasive, and —
//! for the reference/registry indices — the basis of memory safety: guest reference values are
//! indices into bounds-checked stores, never host addresses (§4.3).

/// Index of a heap object within the object registry (§4.3). NOT a host pointer.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct RefIndex(pub u64);

/// Runtime-unique identity of a loaded class = (name, defining loader), interned to a u32 (§8.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct ClassId(pub u32);

/// Virtual-thread id; the unit of Java-level concurrency (§11.1). Diff trees of different tid
/// never merge (§10.5).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct VtId(pub u32);

/// Per-scope local/SSA slot index, range `0..1024` (§9.5).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct SlotId(pub u16);

/// Basic-block id within a method's CFG (§9.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct BlockId(pub u32);

/// ClassLoader identity; part of the class-namespace key (§8.4).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct LoaderId(pub u32);

/// Interned class/member name (§8.4).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct InternedName(pub u32);

/// A newtype usable as a dense index into a [`crate::registry::RegistryVec`] (§8.4).
pub trait RegistryKey: Copy {
    fn from_index(i: usize) -> Self;
    fn index(self) -> usize;
}

impl RegistryKey for ClassId {
    fn from_index(i: usize) -> Self {
        ClassId(i as u32)
    }
    fn index(self) -> usize {
        self.0 as usize
    }
}
impl RegistryKey for BlockId {
    fn from_index(i: usize) -> Self {
        BlockId(i as u32)
    }
    fn index(self) -> usize {
        self.0 as usize
    }
}
impl RegistryKey for RefIndex {
    fn from_index(i: usize) -> Self {
        RefIndex(i as u64)
    }
    fn index(self) -> usize {
        self.0 as usize
    }
}

impl SlotId {
    /// This slot's index into per-scope storage.
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}
