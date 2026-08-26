//! `StackMapTable` decoding (JVMS §4.7.4) into per-offset frames. Locals are produced in the
//! *slot* model (category-2 types expand to `[T, Top]`); the operand stack stays in the *item*
//! model (one entry per value), matching the abstract interpreter in `lib.rs`.

use std::collections::BTreeMap;

use crate::error::VerifyError;
use crate::vtype::VType;

/// A verification frame at a bytecode offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Slot model: category-2 locals occupy two entries (`[value, Top]`).
    pub locals: Vec<VType>,
    /// Item model: one entry per value (a `long`/`double` is a single stack item).
    pub stack: Vec<VType>,
}

struct Cur<'a> {
    b: &'a [u8],
    i: usize,
}

impl Cur<'_> {
    fn u1(&mut self) -> Result<u8, VerifyError> {
        let v = *self.b.get(self.i).ok_or(VerifyError::BadStackMap)?;
        self.i += 1;
        Ok(v)
    }
    fn u2(&mut self) -> Result<u16, VerifyError> {
        Ok(((self.u1()? as u16) << 8) | self.u1()? as u16)
    }
}

fn parse_vti(c: &mut Cur) -> Result<VType, VerifyError> {
    Ok(match c.u1()? {
        0 => VType::Top,
        1 => VType::Int,
        2 => VType::Float,
        3 => VType::Double,
        4 => VType::Long,
        5 => VType::Null,
        6 => VType::UninitializedThis,
        7 => {
            let _class_cp = c.u2()?; // Object_variable_info: opaque reference for now
            VType::Reference
        }
        8 => VType::Uninitialized(c.u2()? as u32),
        _ => return Err(VerifyError::BadStackMap),
    })
}

/// Expand an item-model locals list into the slot model.
fn expand_locals(items: &[VType]) -> Vec<VType> {
    let mut out = Vec::with_capacity(items.len());
    for &t in items {
        out.push(t);
        if t.is_category2() {
            out.push(VType::Top);
        }
    }
    out
}

/// Decode a `StackMapTable` attribute body (starting at `number_of_entries`), given the method's
/// initial locals in the item model. Returns the frame declared at each bytecode offset.
pub fn decode(
    smt: &[u8],
    initial_locals_items: &[VType],
) -> Result<BTreeMap<u32, Frame>, VerifyError> {
    let mut c = Cur { b: smt, i: 0 };
    let entries = c.u2()?;
    let mut locals_items = initial_locals_items.to_vec();
    let mut frames = BTreeMap::new();
    let mut pc: i64 = -1;
    for _ in 0..entries {
        let ft = c.u1()?;
        let (delta, stack): (i64, Vec<VType>) = match ft {
            0..=63 => (ft as i64, vec![]),
            64..=127 => (ft as i64 - 64, vec![parse_vti(&mut c)?]),
            247 => {
                let d = c.u2()? as i64;
                (d, vec![parse_vti(&mut c)?])
            }
            248..=250 => {
                let d = c.u2()? as i64;
                let k = 251 - ft as usize;
                if k > locals_items.len() {
                    return Err(VerifyError::BadStackMap);
                }
                locals_items.truncate(locals_items.len() - k);
                (d, vec![])
            }
            251 => (c.u2()? as i64, vec![]),
            252..=254 => {
                let d = c.u2()? as i64;
                for _ in 0..(ft as usize - 251) {
                    locals_items.push(parse_vti(&mut c)?);
                }
                (d, vec![])
            }
            255 => {
                let d = c.u2()? as i64;
                let nl = c.u2()?;
                let mut items = Vec::with_capacity(nl as usize);
                for _ in 0..nl {
                    items.push(parse_vti(&mut c)?);
                }
                locals_items = items;
                let ns = c.u2()?;
                let mut st = Vec::with_capacity(ns as usize);
                for _ in 0..ns {
                    st.push(parse_vti(&mut c)?);
                }
                (d, st)
            }
            _ => return Err(VerifyError::BadStackMap),
        };
        pc += delta + 1;
        if pc < 0 {
            return Err(VerifyError::BadStackMap);
        }
        frames.insert(
            pc as u32,
            Frame {
                locals: expand_locals(&locals_items),
                stack,
            },
        );
    }
    Ok(frames)
}
