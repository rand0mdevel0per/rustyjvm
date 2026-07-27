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
                // Long/Double occupy two entries; the next index is unusable (JVMS §4.4.5).
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

/// Decode modified UTF-8 (JVMS §4.4.7) into a Rust `String`.
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
            // 2-byte form (also encodes NUL as 0xC0 0x80).
            let b2 = *bytes.get(i + 1).ok_or(ClassFileError::BadUtf8)?;
            let cp = (((b & 0x1F) as u32) << 6) | ((b2 & 0x3F) as u32);
            out.push(char::from_u32(cp).ok_or(ClassFileError::BadUtf8)?);
            i += 2;
        } else if b & 0xF0 == 0xE0 {
            // 3-byte form; a supplementary character is a surrogate pair of two 3-byte forms.
            let b2 = *bytes.get(i + 1).ok_or(ClassFileError::BadUtf8)?;
            let b3 = *bytes.get(i + 2).ok_or(ClassFileError::BadUtf8)?;
            let cp =
                (((b & 0x0F) as u32) << 12) | (((b2 & 0x3F) as u32) << 6) | ((b3 & 0x3F) as u32);
            if (0xD800..=0xDBFF).contains(&cp) {
                // High surrogate: consume the following 3-byte low surrogate and combine.
                let c4 = *bytes.get(i + 3).ok_or(ClassFileError::BadUtf8)?;
                let c5 = *bytes.get(i + 4).ok_or(ClassFileError::BadUtf8)?;
                let c6 = *bytes.get(i + 5).ok_or(ClassFileError::BadUtf8)?;
                if c4 & 0xF0 != 0xE0 {
                    return Err(ClassFileError::BadUtf8);
                }
                let lo = (((c4 & 0x0F) as u32) << 12)
                    | (((c5 & 0x3F) as u32) << 6)
                    | ((c6 & 0x3F) as u32);
                if !(0xDC00..=0xDFFF).contains(&lo) {
                    return Err(ClassFileError::BadUtf8);
                }
                let combined = 0x1_0000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                out.push(char::from_u32(combined).ok_or(ClassFileError::BadUtf8)?);
                i += 6;
            } else {
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
    fn mutf8_ascii_and_two_byte_nul() {
        assert_eq!(decode_mutf8(b"Slice").unwrap(), "Slice");
        assert_eq!(decode_mutf8(b"(IIJF)I").unwrap(), "(IIJF)I");
        // MUTF-8 encodes NUL as 0xC0 0x80.
        assert_eq!(decode_mutf8(&[0xC0, 0x80]).unwrap(), "\0");
        // A bare NUL byte is invalid.
        assert_eq!(decode_mutf8(&[0x00]), Err(ClassFileError::BadUtf8));
    }
}
