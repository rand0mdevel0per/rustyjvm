//! Attributes (JVMS §4.7). Increment 1 fully parses the `Code` attribute (JVMS §4.7.3); all other
//! attributes — including `StackMapTable` (decoded by `rjava-verify`), `LineNumberTable`, etc. —
//! are retained as raw bytes for later stages.

use crate::constant_pool::ConstantPool;
use crate::error::ClassFileError;
use crate::reader::Reader;

/// A class-file attribute.
#[derive(Debug, Clone)]
pub enum Attribute {
    /// The `Code` attribute of a method (JVMS §4.7.3).
    Code(CodeAttr),
    /// Any other attribute, kept verbatim. `name_index` refers to a `Utf8` constant.
    Raw { name_index: u16, bytes: Vec<u8> },
}

impl Attribute {
    /// If this is a `Raw` attribute whose name resolves to `name`.
    pub fn raw_named<'a>(&'a self, cp: &ConstantPool, name: &str) -> Option<&'a [u8]> {
        match self {
            Attribute::Raw { name_index, bytes } if cp.utf8(*name_index) == Some(name) => {
                Some(bytes)
            }
            _ => None,
        }
    }
}

/// A method's `Code` attribute (JVMS §4.7.3).
#[derive(Debug, Clone)]
pub struct CodeAttr {
    pub max_stack: u16,
    pub max_locals: u16,
    pub code: Vec<u8>,
    pub exception_table: Vec<ExceptionTableEntry>,
    pub attributes: Vec<Attribute>,
}

/// One row of a `Code` attribute's exception table (JVMS §4.7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExceptionTableEntry {
    pub start_pc: u16,
    pub end_pc: u16,
    pub handler_pc: u16,
    /// A `Class` constant index, or 0 for a catch-all (`finally`).
    pub catch_type: u16,
}

impl CodeAttr {
    fn parse(r: &mut Reader, cp: &ConstantPool) -> Result<CodeAttr, ClassFileError> {
        let max_stack = r.u2()?;
        let max_locals = r.u2()?;
        let code_length = r.u4()? as usize;
        let code = r.bytes(code_length)?.to_vec();
        let etl = r.u2()?;
        let mut exception_table = Vec::with_capacity(etl as usize);
        for _ in 0..etl {
            exception_table.push(ExceptionTableEntry {
                start_pc: r.u2()?,
                end_pc: r.u2()?,
                handler_pc: r.u2()?,
                catch_type: r.u2()?,
            });
        }
        let attributes = parse_attributes(r, cp)?;
        Ok(CodeAttr {
            max_stack,
            max_locals,
            code,
            exception_table,
            attributes,
        })
    }
}

/// Parse a single `attribute_info` (JVMS §4.7).
fn parse_attribute(r: &mut Reader, cp: &ConstantPool) -> Result<Attribute, ClassFileError> {
    let name_index = r.u2()?;
    let length = r.u4()? as usize;
    let is_code = cp.utf8(name_index) == Some("Code");
    // Bound the body to `length` so a nested parse cannot overrun the attribute.
    let body = r.bytes(length)?;
    if is_code {
        let mut cr = Reader::new(body);
        Ok(Attribute::Code(CodeAttr::parse(&mut cr, cp)?))
    } else {
        Ok(Attribute::Raw {
            name_index,
            bytes: body.to_vec(),
        })
    }
}

/// Parse an `attributes_count` followed by that many `attribute_info` (JVMS §4.7).
pub fn parse_attributes(
    r: &mut Reader,
    cp: &ConstantPool,
) -> Result<Vec<Attribute>, ClassFileError> {
    let count = r.u2()?;
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        out.push(parse_attribute(r, cp)?);
    }
    Ok(out)
}
