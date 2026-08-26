//! Bytecode instruction decoding (JVMS §6). Increment 1 decodes exactly the vertical-slice opcode
//! set (constants, typed loads/stores, integer/long/float arithmetic, numeric conversions, 3-way
//! compares, `if<cond>`/`if_icmp<cond>`/`goto`, `ireturn`). Any other opcode is rejected as
//! `UnsupportedOpcode` — safe (reject-unknown) and extended in later increments. Every read is
//! bounds-checked, so decoding never panics (§4.3, §23.3).

use std::collections::HashMap;

use crate::error::VerifyError;

/// A decoded instruction. `arg` carries the sole operand where one exists: a local/constant-pool
/// index, an immediate (`bipush`/`sipush`), or — for branches — the *absolute* target pc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insn {
    pub pc: u32,
    pub op: u8,
    pub arg: i64,
    pub len: u8,
}

impl Insn {
    /// The absolute branch target, for `if<cond>`/`if_icmp<cond>`/`ifnull`/`ifnonnull`/`goto`.
    pub fn branch_target(&self) -> Option<u32> {
        match self.op {
            0x99..=0xa4 | 0xa7 | 0xc6 | 0xc7 => Some(self.arg as u32),
            _ => None,
        }
    }
    /// True when control cannot fall through to the next instruction: `goto` and every return.
    pub fn is_unconditional_end(&self) -> bool {
        matches!(self.op, 0xa7) || (0xac..=0xb1).contains(&self.op)
    }
}

fn u1_at(code: &[u8], i: usize) -> Result<u8, VerifyError> {
    code.get(i).copied().ok_or(VerifyError::Truncated(i as u32))
}
fn s1_at(code: &[u8], i: usize) -> Result<i64, VerifyError> {
    Ok(u1_at(code, i)? as i8 as i64)
}
fn u2_at(code: &[u8], i: usize) -> Result<i64, VerifyError> {
    Ok(((u1_at(code, i)? as i64) << 8) | u1_at(code, i + 1)? as i64)
}
fn s2_at(code: &[u8], i: usize) -> Result<i64, VerifyError> {
    Ok(u2_at(code, i)? as i16 as i64)
}

/// Decode a `Code` byte array into instructions.
pub fn decode(code: &[u8]) -> Result<Vec<Insn>, VerifyError> {
    let mut out = Vec::new();
    let mut pc = 0usize;
    while pc < code.len() {
        let op = code[pc];
        let (arg, len): (i64, u8) = match op {
            // No-operand (len 1): nop; iconst..fconst; iload_<n>/lload_<n>/fload_<n>;
            // istore_<n>/lstore_<n>/fstore_<n>; int/long/float add/sub/mul; idiv/irem/ineg;
            // i2l/i2f/l2i/l2f/f2i/f2l; lcmp/fcmpl/fcmpg; ireturn.
            0x00
            | 0x01
            | 0x02..=0x0d
            | 0x1a..=0x2d
            | 0x3b..=0x4e
            | 0x59
            | 0x60..=0x62
            | 0x64..=0x66
            | 0x68..=0x6a
            | 0x6c
            | 0x70
            | 0x74
            | 0x7e
            | 0x85
            | 0x86
            | 0x88
            | 0x89
            | 0x8b
            | 0x8c
            | 0x94..=0x96
            | 0xac
            | 0xad
            | 0xb0
            | 0xb1 => (0, 1),
            0x84 => {
                // iinc index, const: pack the u1 index and i1 const into `arg`.
                let index = u1_at(code, pc + 1)? as i64;
                let delta = s1_at(code, pc + 2)?;
                ((index << 8) | ((delta as u8) as i64), 3)
            }
            0x10 => (s1_at(code, pc + 1)?, 2), // bipush
            0x11 => (s2_at(code, pc + 1)?, 3), // sipush
            0x15..=0x17 | 0x19 => (u1_at(code, pc + 1)? as i64, 2), // iload/lload/fload/aload
            0x36..=0x38 | 0x3a => (u1_at(code, pc + 1)? as i64, 2), // istore/lstore/fstore/astore
            0x12 => (u1_at(code, pc + 1)? as i64, 2), // ldc
            0x13 | 0x14 => (u2_at(code, pc + 1)?, 3), // ldc_w / ldc2_w
            // invoke* / new / field access / instanceof / checkcast (cp index)
            0xb2..=0xb8 | 0xbb | 0xc0 | 0xc1 => (u2_at(code, pc + 1)?, 3),
            0x99..=0xa4 | 0xa7 | 0xc6 | 0xc7 => (pc as i64 + s2_at(code, pc + 1)?, 3), // branches
            _ => return Err(VerifyError::UnsupportedOpcode { op, pc: pc as u32 }),
        };
        out.push(Insn {
            pc: pc as u32,
            op,
            arg,
            len,
        });
        pc += len as usize;
    }
    Ok(out)
}

/// Map every instruction's pc to its index in the decoded stream.
pub fn pc_index(insns: &[Insn]) -> HashMap<u32, usize> {
    insns
        .iter()
        .enumerate()
        .map(|(i, ins)| (ins.pc, i))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_and_computes_branch_targets() {
        // iload_0(0x1a); ifne +5 (0x9a 0x00 0x05); ireturn(0xac)
        let code = [0x1a, 0x9a, 0x00, 0x05, 0xac];
        let insns = decode(&code).unwrap();
        assert_eq!(insns.len(), 3);
        assert_eq!(insns[0].op, 0x1a);
        assert_eq!(insns[1].branch_target(), Some(1 + 5)); // pc 1 + offset 5
        assert_eq!(insns[2].op, 0xac);
    }

    #[test]
    fn rejects_unknown_opcode_without_panic() {
        assert!(matches!(
            decode(&[0xfe]),
            Err(VerifyError::UnsupportedOpcode { op: 0xfe, pc: 0 })
        ));
    }

    #[test]
    fn truncated_operand_is_error() {
        assert!(matches!(decode(&[0x10]), Err(VerifyError::Truncated(_)))); // bipush, missing byte
    }
}
