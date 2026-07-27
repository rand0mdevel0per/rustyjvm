//! Errors raised while parsing a class file (RJVM-SPEC-001 §7.1: integrity/well-formedness, a
//! distinct concern from JVMS §4.10 verification which `rjava-verify` performs). All are HOST
//! errors; a malformed file yields an error, never a panic or host memory unsafety (§4.3, §23.3).

/// A class-file parsing error.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ClassFileError {
    #[error("unexpected end of input at byte offset {0}")]
    Eof(usize),
    #[error("bad magic {0:#010x} (expected 0xCAFEBABE)")]
    BadMagic(u32),
    #[error("unknown constant-pool tag {tag} at index {index}")]
    UnknownTag { tag: u8, index: u16 },
    #[error("invalid modified-UTF-8 in a Utf8 constant")]
    BadUtf8,
    #[error("constant-pool index {0} out of range or wrong type")]
    BadCpIndex(u16),
    #[error("malformed attribute: {0}")]
    BadAttribute(&'static str),
}
