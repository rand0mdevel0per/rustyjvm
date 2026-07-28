//! Verification types (JVMS §4.10.1.2) and descriptor parsing. Increment 1 needs the numeric
//! lattice (Int/Long/Float/Double/Top); references are modelled opaquely for forward-compatibility
//! (the slice has none). `long`/`double` are category-2: one operand-stack item, two local slots.

use crate::error::VerifyError;

/// A verification type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VType {
    /// The unusable/uninitialised top of the lattice; also the high half of a category-2 local.
    Top,
    Int,
    Float,
    Long,
    Double,
    /// Any object/array reference (opaque until reference subtyping is needed).
    Reference,
    Null,
    UninitializedThis,
    /// An object created by `new` at the given bytecode offset, not yet `<init>`-ed.
    Uninitialized(u32),
}

impl VType {
    /// Number of local/stack *slots* this type occupies (category-2 types take two).
    pub fn size(self) -> u16 {
        matches!(self, VType::Long | VType::Double) as u16 + 1
    }
    pub fn is_category2(self) -> bool {
        matches!(self, VType::Long | VType::Double)
    }
}

/// Whether a value of type `from` may be used where `to` is expected (JVMS §4.10.1.2).
pub fn is_assignable(from: VType, to: VType) -> bool {
    if to == VType::Top || from == to {
        return true;
    }
    matches!(
        (from, to),
        (VType::Null, VType::Reference)
            | (VType::Reference, VType::Reference)
            | (VType::UninitializedThis, VType::Reference)
            | (VType::Uninitialized(_), VType::Reference)
    )
}

/// The verification type a field descriptor denotes (JVMS §4.3.2). Returns the type and the index
/// just past it.
fn parse_field_type(b: &[u8], mut i: usize) -> Result<(VType, usize), VerifyError> {
    let c = *b.get(i).ok_or(VerifyError::BadDescriptor)?;
    let t = match c {
        b'B' | b'C' | b'I' | b'S' | b'Z' => VType::Int,
        b'J' => VType::Long,
        b'F' => VType::Float,
        b'D' => VType::Double,
        b'L' => {
            // Object type: skip to the terminating ';'.
            while *b.get(i).ok_or(VerifyError::BadDescriptor)? != b';' {
                i += 1;
            }
            VType::Reference
        }
        b'[' => {
            // Array: skip dimensions then the component type; the result is a reference.
            while *b.get(i).ok_or(VerifyError::BadDescriptor)? == b'[' {
                i += 1;
            }
            if *b.get(i).ok_or(VerifyError::BadDescriptor)? == b'L' {
                while *b.get(i).ok_or(VerifyError::BadDescriptor)? != b';' {
                    i += 1;
                }
            }
            VType::Reference
        }
        _ => return Err(VerifyError::BadDescriptor),
    };
    Ok((t, i + 1))
}

/// Parse a method descriptor into its argument types and return type (`None` = `void`).
pub fn parse_method_descriptor(desc: &str) -> Result<(Vec<VType>, Option<VType>), VerifyError> {
    let b = desc.as_bytes();
    if b.first() != Some(&b'(') {
        return Err(VerifyError::BadDescriptor);
    }
    let mut i = 1;
    let mut args = Vec::new();
    while *b.get(i).ok_or(VerifyError::BadDescriptor)? != b')' {
        let (t, ni) = parse_field_type(b, i)?;
        args.push(t);
        i = ni;
    }
    i += 1; // past ')'
    let ret = if b.get(i) == Some(&b'V') {
        None
    } else {
        Some(parse_field_type(b, i)?.0)
    };
    Ok((args, ret))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_of_slice_arith() {
        let (args, ret) = parse_method_descriptor("(IIJF)I").unwrap();
        assert_eq!(
            args,
            vec![VType::Int, VType::Int, VType::Long, VType::Float]
        );
        assert_eq!(ret, Some(VType::Int));
    }

    #[test]
    fn descriptor_refs_and_void() {
        let (args, ret) = parse_method_descriptor("(Ljava/lang/String;[IJ)V").unwrap();
        assert_eq!(args, vec![VType::Reference, VType::Reference, VType::Long]);
        assert_eq!(ret, None);
        assert!(parse_method_descriptor("nonsense").is_err());
    }

    #[test]
    fn assignability_basics() {
        assert!(is_assignable(VType::Int, VType::Int));
        assert!(is_assignable(VType::Int, VType::Top));
        assert!(!is_assignable(VType::Top, VType::Int));
        assert!(!is_assignable(VType::Int, VType::Long));
        assert!(is_assignable(VType::Null, VType::Reference));
    }
}
