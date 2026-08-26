//! Errors raised by the JVMS §4.10 verifier. Every violation is a `VerifyError` (surfaced to the
//! guest as `VerifyError` at link time, STD-CODE-4); the verifier never panics, even on
//! adversarial input (§7.1, §23.3).

/// A bytecode-verification failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    #[error("method has no Code attribute")]
    NoCode,
    #[error("unsupported/unknown opcode {op:#04x} at pc {pc} (increment-1 subset)")]
    UnsupportedOpcode { op: u8, pc: u32 },
    #[error("bytecode truncated near pc {0}")]
    Truncated(u32),
    #[error("branch at pc {at} targets {target}, which is not an instruction boundary")]
    BadBranchTarget { at: u32, target: i64 },
    #[error("operand stack underflow at pc {0}")]
    StackUnderflow(u32),
    #[error("operand stack exceeds max_stack at pc {0}")]
    StackOverflow(u32),
    #[error("type mismatch at pc {pc}: {what}")]
    TypeMismatch { pc: u32, what: &'static str },
    #[error("local index {index} out of range at pc {pc}")]
    BadLocal { pc: u32, index: u16 },
    #[error("no stack-map frame for reachable pc {0}")]
    MissingFrame(u32),
    #[error("stack-map frame mismatch entering pc {0}")]
    FrameMismatch(u32),
    #[error("malformed method/field descriptor")]
    BadDescriptor,
    #[error("malformed StackMapTable")]
    BadStackMap,
    #[error("return type mismatch at pc {0}")]
    BadReturn(u32),
}
