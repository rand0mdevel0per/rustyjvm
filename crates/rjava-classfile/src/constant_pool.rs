//! The constant pool (JVMS §4.4). One-indexed: index 0 and the slot following each `Long`/`Double`
//! hold an [`Constant::Unusable`] sentinel (JVMS §4.4.5). `Utf8` constants are decoded from
//! modified UTF-8 (MUTF-8, JVMS §4.4.7), which differs from standard UTF-8 for embedded NULs and
//! supplementary characters.

use crate::error::ClassFileError;
use crate::reader::Reader;

/// A single constant-pool entry.
#[derive(Debug, Clone, PartialEq)]
pub enum Constant {
    Utf8(String),
    Integer(i32),
    Float(f32),
    Long(i64),
    Double(f64),
    Class {
        name_index: u16,
    },
    String {
        string_index: u16,
    },
    FieldRef {
        class_index: u16,
        name_and_type_index: u16,
    },
    MethodRef {
        class_index: u16,
        name_and_type_index: u16,
    },
    InterfaceMethodRef {
        class_index: u16,
        name_and_type_index: u16,
    },
    NameAndType {
        name_index: u16,
        descriptor_index: u16,
    },
    MethodHandle {
        reference_kind: u8,
        reference_index: u16,
    },
    MethodType {
        descriptor_index: u16,
    },
    Dynamic {
        bootstrap_method_attr_index: u16,
        name_and_type_index: u16,
    },
    InvokeDynamic {
        bootstrap_method_attr_index: u16,
        name_and_type_index: u16,
    },
    Module {
        name_index: u16,
    },
    Package {
        name_index: u16,
    },
    /// Index 0, and the second slot of a `Long`/`Double`.
    Unusable,
}

/// The parsed constant pool.
#[derive(Debug, Clone)]
pub struct ConstantPool {
    entries: Vec<Constant>, // one-indexed; entries[0] == Unusable
}

impl ConstantPool {
    /// Parse `constant_pool_count` and the entries that follow (JVMS §4.4).
    pub fn parse(r: &mut Reader) -> Result<ConstantPool, ClassFileError> {
        let count = r.u2()?; // constant_pool_count; there are count-1 entries
        let mut entries = Vec::with_capacity(count as usize);
        entries.push(Constant::Unusable); // index 0 sentinel
        let mut i = 1u16;
        while i < count {
            let tag = r.u1()?;
            let c = match tag {
                1 => {
                    let len = r.u2()? as usize;
                    Constant::Utf8(decode_mutf8(r.bytes(len)?)?)
                }
                3 => Constant::Integer(r.u4()? as i32),
                4 => Constant::Float(f32::from_bits(r.u4()?)),
                5 => Constant::Long(r.u8()? as i64),
                6 => Constant::Double(f64::from_bits(r.u8()?)),
                7 => Constant::Class {
                    name_index: r.u2()?,
                },
                8 => Constant::String {
                    string_index: r.u2()?,
                },
                9 => Constant::FieldRef {
                    class_index: r.u2()?,
                    name_and_type_index: r.u2()?,
                },
                10 => Constant::MethodRef {
                    class_index: r.u2()?,
                    name_and_type_index: r.u2()?,
                },
                11 => Constant::InterfaceMethodRef {
                    class_index: r.u2()?,
                    name_and_type_index: r.u2()?,
                },
                12 => Constant::NameAndType {
                    name_index: r.u2()?,
                    descriptor_index: r.u2()?,
                },
                15 => Constant::MethodHandle {
                    reference_kind: r.u1()?,
                    reference_index: r.u2()?,
                },
                16 => Constant::MethodType {
                    descriptor_index: r.u2()?,
                },
                17 => Constant::Dynamic {
                    bootstrap_method_attr_index: r.u2()?,
                    name_and_type_index: r.u2()?,
                },
                18 => Constant::InvokeDynamic {
                    bootstrap_method_attr_index: r.u2()?,
                    name_and_type_index: r.u2()?,
                },
                19 => Constant::Module {
                    name_index: r.u2()?,
                },
                20 => Constant::Package {
                    name_index: r.u2()?,
                },
                _ => return Err(ClassFileError::UnknownTag { tag, index: i }),
            };
            let wide = matches!(c, Constant::Long(_) | Constant::Double(_));
            entries.push(c);
            i += 1;
            if wide {
                // Long/Double occupy two entries; the next index is unusable (JVMS §4.4.5). A wide
                // entry in the final slot would run past `constant_pool_count`, which makes the
                // class malformed — reject rather than silently over-filling the pool.
                if i >= count {
                    return Err(ClassFileError::BadCpIndex(i));
                }
                entries.push(Constant::Unusable);
                i += 1;
            }
        }
        Ok(ConstantPool { entries })
    }

    /// Number of entries (including sentinels), i.e. `constant_pool_count`.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.len() <= 1
    }

    /// The constant at `index`, if in range.
    pub fn get(&self, index: u16) -> Option<&Constant> {
        self.entries.get(index as usize)
    }

    /// A `Utf8` constant's text.
    pub fn utf8(&self, index: u16) -> Option<&str> {
        match self.get(index)? {
            Constant::Utf8(s) => Some(s),
            _ => None,
        }
    }

    /// A `Class` constant's internal name (e.g. `java/lang/Object`).
    pub fn class_name(&self, index: u16) -> Option<&str> {
        match self.get(index)? {
            Constant::Class { name_index } => self.utf8(*name_index),
            _ => None,
        }
    }
}

/// A MUTF-8 continuation byte must be `10xxxxxx`.
#[inline]
fn cont(bytes: &[u8], i: usize) -> Result<u32, ClassFileError> {
    let b = *bytes.get(i).ok_or(ClassFileError::BadUtf8)?;
    if b & 0xC0 != 0x80 {
        return Err(ClassFileError::BadUtf8);
    }
    Ok((b & 0x3F) as u32)
}

/// Decode modified UTF-8 (JVMS §4.4.7) into a Rust `String`.
///
/// Validation is strict — a class file is untrusted input (§7.1), so a malformed constant must be
/// rejected rather than silently decoded into a string a conforming JVM would never produce:
/// continuation bytes must be `10xxxxxx`, and overlong encodings are refused (with the single
/// MUTF-8-mandated exception `C0 80`, which encodes NUL).
///
/// Note: MUTF-8 can encode an unpaired surrogate, which a Java `String` (UTF-16) can hold but a
/// Rust `String` cannot. Such input is rejected rather than lossily substituted.
fn decode_mutf8(bytes: &[u8]) -> Result<String, ClassFileError> {
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0 {
            return Err(ClassFileError::BadUtf8); // NUL is never a single byte in MUTF-8
        }
        if b < 0x80 {
            out.push(b as char);
            i += 1;
        } else if b & 0xE0 == 0xC0 {
            // 2-byte form. `C0 80` is MUTF-8's NUL; every other sub-0x80 result is overlong.
            let cp = (((b & 0x1F) as u32) << 6) | cont(bytes, i + 1)?;
            if cp < 0x80 && !(b == 0xC0 && bytes[i + 1] == 0x80) {
                return Err(ClassFileError::BadUtf8);
            }
            out.push(char::from_u32(cp).ok_or(ClassFileError::BadUtf8)?);
            i += 2;
        } else if b & 0xF0 == 0xE0 {
            // 3-byte form; a supplementary character is a surrogate pair of two 3-byte forms.
            let cp = (((b & 0x0F) as u32) << 12) | (cont(bytes, i + 1)? << 6) | cont(bytes, i + 2)?;
            if cp < 0x800 {
                return Err(ClassFileError::BadUtf8); // overlong
            }
            if (0xD800..=0xDBFF).contains(&cp) {
                // High surrogate: consume the following 3-byte low surrogate and combine.
                let c4 = *bytes.get(i + 3).ok_or(ClassFileError::BadUtf8)?;
                if c4 & 0xF0 != 0xE0 {
                    return Err(ClassFileError::BadUtf8);
                }
                let lo =
                    (((c4 & 0x0F) as u32) << 12) | (cont(bytes, i + 4)? << 6) | cont(bytes, i + 5)?;
                if !(0xDC00..=0xDFFF).contains(&lo) {
                    return Err(ClassFileError::BadUtf8);
                }
                let combined = 0x1_0000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                out.push(char::from_u32(combined).ok_or(ClassFileError::BadUtf8)?);
                i += 6;
            } else {
                // Rejects an unpaired low surrogate (not representable in a Rust `String`).
                out.push(char::from_u32(cp).ok_or(ClassFileError::BadUtf8)?);
                i += 3;
            }
        } else {
            return Err(ClassFileError::BadUtf8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_wide_constant_in_the_final_slot() {
        // Regression for a review finding: a Long/Double occupying the last declared slot runs past
        // `constant_pool_count` (JVMS §4.4.5 makes the class malformed) and would also break the
        // documented `len() == constant_pool_count`.
        let mut b = Vec::new();
        b.extend_from_slice(&2u16.to_be_bytes()); // constant_pool_count = 2 → exactly one entry
        b.push(5); // CONSTANT_Long
        b.extend_from_slice(&7i64.to_be_bytes()); // ...which needs two slots — overrun
        let mut r = Reader::new(&b);
        assert!(matches!(
            ConstantPool::parse(&mut r),
            Err(ClassFileError::BadCpIndex(_))
        ));

        // With room for both slots it parses, and `len()` equals the declared count.
        let mut ok = Vec::new();
        ok.extend_from_slice(&3u16.to_be_bytes()); // count = 3 → entry 1 (wide) + sentinel 2
        ok.push(5);
        ok.extend_from_slice(&7i64.to_be_bytes());
        let mut r = Reader::new(&ok);
        let cp = ConstantPool::parse(&mut r).expect("well-formed wide entry parses");
        assert_eq!(cp.len(), 3);
        assert_eq!(cp.get(1), Some(&Constant::Long(7)));
        assert_eq!(cp.get(2), Some(&Constant::Unusable));
    }

    #[test]
    fn mutf8_ascii_and_two_byte_nul() {
        assert_eq!(decode_mutf8(b"Slice").unwrap(), "Slice");
        assert_eq!(decode_mutf8(b"(IIJF)I").unwrap(), "(IIJF)I");
        // MUTF-8 encodes NUL as 0xC0 0x80.
        assert_eq!(decode_mutf8(&[0xC0, 0x80]).unwrap(), "\0");
        // A bare NUL byte is invalid.
        assert_eq!(decode_mutf8(&[0x00]), Err(ClassFileError::BadUtf8));
    }

    #[test]
    fn mutf8_valid_multibyte_roundtrips() {
        // 2-byte (U+00E9 é), 3-byte (U+4E2D 中), and a surrogate pair (U+1F600 😀).
        assert_eq!(decode_mutf8(&[0xC3, 0xA9]).unwrap(), "é");
        assert_eq!(decode_mutf8(&[0xE4, 0xB8, 0xAD]).unwrap(), "中");
        assert_eq!(
            decode_mutf8(&[0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80]).unwrap(),
            "😀"
        );
    }

    #[test]
    fn mutf8_rejects_malformed_and_overlong() {
        // Regression for a review finding: continuation bytes must be 10xxxxxx, and overlong
        // encodings are forbidden (except the mandated C0 80 NUL). A class file is untrusted
        // input, so these must be rejected, not silently decoded (§7.1).
        for bad in [
            &[0xC2, 0x41][..],             // 2-byte: second byte is not a continuation
            &[0xE1, 0x41, 0x42][..],       // 3-byte: neither trailing byte is a continuation
            &[0xC0, 0x81][..],             // overlong 2-byte (only C0 80 is allowed)
            &[0xE0, 0x80, 0x81][..],       // overlong 3-byte
            &[0xC1, 0xBF][..],             // overlong 2-byte encoding of 0x7F
            &[0xF0, 0x9F, 0x98, 0x80][..], // 4-byte UTF-8 is not valid MUTF-8
            &[0xED, 0xB8, 0x80][..],       // unpaired low surrogate
            &[0xED, 0xA0, 0xBD][..],       // unpaired high surrogate (truncated pair)
            &[0x80][..],                   // stray continuation byte
        ] {
            assert_eq!(
                decode_mutf8(bad),
                Err(ClassFileError::BadUtf8),
                "must reject {bad:02X?}"
            );
        }
    }
}
