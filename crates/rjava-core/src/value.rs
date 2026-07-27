//! The 128-bit tagged value — the uniform representation of every guest value on the operand
//! stack, in local slots, and in object fields (RJVM-SPEC-001 §4.1–4.2).
//!
//! Layout (§4.1):
//! ```text
//!  bit 127 .......................... bit 8 | bit 7 .. bit 0
//!  |            payload (120 bits)          |  type tag (8) |
//! ```
//! The tag is maintained exclusively by the interpreter and is immutable by guest code. A
//! uniform 128-bit representation removes per-slot type branching in storage layout; the JIT
//! later evaporates both the tag and the tag check within verified type-safe regions (§13.5).

use crate::ids::RefIndex;

/// 8-bit type tag identifying the schema of a [`Val128`] payload (§4.1). Guest-immutable.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum Tag {
    /// 32-bit int. Per JVMS stack typing this also carries boolean/byte/char/short values.
    I32 = 0,
    /// 64-bit long.
    I64 = 1,
    /// 32-bit float (IEEE-754; strict — §13.6).
    F32 = 2,
    /// 64-bit double (IEEE-754; strict — §13.6).
    F64 = 3,
    /// Shared (non-exclusive) reference: a registry index (§4.2; S2/S3).
    Ptr = 4,
    /// Move-only (exclusive-ownership) reference: a registry index (§4.2; S1).
    Handle = 5,
    /// The `null` reference.
    Null = 6,
    /// `returnAddress` (jsr/ret; legacy class files only).
    ReturnAddress = 7,
}

impl Tag {
    /// Decode a tag byte, or `None` if it is not a defined tag.
    pub const fn from_u8(b: u8) -> Option<Tag> {
        Some(match b {
            0 => Tag::I32,
            1 => Tag::I64,
            2 => Tag::F32,
            3 => Tag::F64,
            4 => Tag::Ptr,
            5 => Tag::Handle,
            6 => Tag::Null,
            7 => Tag::ReturnAddress,
            _ => return None,
        })
    }
    /// True for the two reference tags (`ptr`/`handle`).
    pub const fn is_ref(self) -> bool {
        matches!(self, Tag::Ptr | Tag::Handle)
    }
}

/// Number of payload bits (§4.1). Implementations MUST NOT assume more.
pub const PAYLOAD_BITS: u32 = 120;
const TAG_BITS: u32 = 8;
const TAG_MASK: u128 = 0xFF;

/// A fixed 128-bit tagged value (§4.1). 16-byte aligned so it can be the unit of a 128-bit atomic
/// where required (§12.6); the copied value itself is immutable (mutable per-object metadata lives
/// in the registry control block — §4.4).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C, align(16))]
pub struct Val128(u128);

impl Val128 {
    #[inline]
    const fn pack(payload: u128, tag: Tag) -> Val128 {
        // `payload` MUST fit in 120 bits; our constructors guarantee it.
        Val128((payload << TAG_BITS) | (tag as u128))
    }

    /// The 8-bit tag.
    #[inline]
    pub fn tag(self) -> Tag {
        Tag::from_u8((self.0 & TAG_MASK) as u8).expect("Val128 always carries a valid tag")
    }

    /// The 120-bit payload (tag stripped).
    #[inline]
    pub const fn payload(self) -> u128 {
        self.0 >> TAG_BITS
    }

    #[inline]
    pub const fn null() -> Val128 {
        Val128::pack(0, Tag::Null)
    }
    #[inline]
    pub const fn from_i32(v: i32) -> Val128 {
        Val128::pack(v as u32 as u128, Tag::I32)
    }
    #[inline]
    pub const fn from_i64(v: i64) -> Val128 {
        Val128::pack(v as u64 as u128, Tag::I64)
    }
    #[inline]
    pub fn from_f32(v: f32) -> Val128 {
        Val128::pack(v.to_bits() as u128, Tag::F32)
    }
    #[inline]
    pub fn from_f64(v: f64) -> Val128 {
        Val128::pack(v.to_bits() as u128, Tag::F64)
    }
    /// A shared reference (`ptr`) to registry index `idx` (§4.2; S2/S3).
    #[inline]
    pub const fn ptr(idx: RefIndex) -> Val128 {
        Val128::pack(idx.0 as u128, Tag::Ptr)
    }
    /// A move-only reference (`handle`) to registry index `idx` (§4.2; S1).
    #[inline]
    pub const fn handle(idx: RefIndex) -> Val128 {
        Val128::pack(idx.0 as u128, Tag::Handle)
    }

    #[inline]
    pub fn as_i32(self) -> i32 {
        debug_assert_eq!(self.tag(), Tag::I32);
        self.payload() as u32 as i32
    }
    #[inline]
    pub fn as_i64(self) -> i64 {
        debug_assert_eq!(self.tag(), Tag::I64);
        self.payload() as u64 as i64
    }
    #[inline]
    pub fn as_f32(self) -> f32 {
        debug_assert_eq!(self.tag(), Tag::F32);
        f32::from_bits(self.payload() as u32)
    }
    #[inline]
    pub fn as_f64(self) -> f64 {
        debug_assert_eq!(self.tag(), Tag::F64);
        f64::from_bits(self.payload() as u64)
    }
    /// The registry index of a `ptr`/`handle` (§4.3).
    #[inline]
    pub fn ref_index(self) -> RefIndex {
        debug_assert!(self.tag().is_ref());
        RefIndex(self.payload() as u64)
    }

    /// Raw 128-bit bits (for atomic storage — §12.6).
    #[inline]
    pub const fn to_bits(self) -> u128 {
        self.0
    }
    /// Reconstruct from raw bits (caller guarantees a valid encoding).
    #[inline]
    pub const fn from_bits(bits: u128) -> Val128 {
        Val128(bits)
    }
}

impl core::fmt::Debug for Val128 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.tag() {
            Tag::I32 => write!(f, "i32({})", self.as_i32()),
            Tag::I64 => write!(f, "i64({})", self.as_i64()),
            Tag::F32 => write!(f, "f32({})", self.as_f32()),
            Tag::F64 => write!(f, "f64({})", self.as_f64()),
            Tag::Ptr => write!(f, "ptr(#{})", self.payload()),
            Tag::Handle => write!(f, "handle(#{})", self.payload()),
            Tag::Null => write!(f, "null"),
            Tag::ReturnAddress => write!(f, "retaddr({})", self.payload()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_16x16() {
        assert_eq!(core::mem::size_of::<Val128>(), 16);
        assert_eq!(core::mem::align_of::<Val128>(), 16);
    }

    #[test]
    fn tag_byte_roundtrip() {
        for b in 0..8u8 {
            assert_eq!(Tag::from_u8(b).unwrap() as u8, b);
        }
        assert!(Tag::from_u8(8).is_none());
    }

    #[test]
    fn i32_roundtrip() {
        for v in [0, 1, -1, i32::MIN, i32::MAX, 12345, -98765] {
            let x = Val128::from_i32(v);
            assert_eq!(x.tag(), Tag::I32);
            assert_eq!(x.as_i32(), v);
        }
    }

    #[test]
    fn i64_roundtrip() {
        for v in [0i64, 1, -1, i64::MIN, i64::MAX] {
            let x = Val128::from_i64(v);
            assert_eq!(x.tag(), Tag::I64);
            assert_eq!(x.as_i64(), v);
        }
    }

    #[test]
    fn f32_roundtrip_preserves_raw_bits() {
        for v in [
            0.0f32,
            -0.0,
            1.5,
            -3.25,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
        ] {
            let x = Val128::from_f32(v);
            assert_eq!(x.tag(), Tag::F32);
            // Bit-exact per IEEE-754 strictness (§13.6): NaN payload and -0.0 must survive.
            assert_eq!(x.as_f32().to_bits(), v.to_bits());
        }
    }

    #[test]
    fn f64_roundtrip_preserves_raw_bits() {
        for v in [
            0.0f64,
            -0.0,
            1.5,
            -3.25,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
        ] {
            let x = Val128::from_f64(v);
            assert_eq!(x.tag(), Tag::F64);
            assert_eq!(x.as_f64().to_bits(), v.to_bits());
        }
    }

    #[test]
    fn refs_and_null() {
        let i = RefIndex(0xDEAD_BEEF);
        let p = Val128::ptr(i);
        assert_eq!(p.tag(), Tag::Ptr);
        assert_eq!(p.ref_index(), i);
        let h = Val128::handle(i);
        assert_eq!(h.tag(), Tag::Handle);
        assert_eq!(h.ref_index(), i);
        assert_eq!(Val128::null().tag(), Tag::Null);
    }
}
