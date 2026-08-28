//! rjava-core — the retrofit-hostile foundation of RustyJVM (RJVM-SPEC-001 §3.2, §4, §5, §9, §12).
//!
//! Every guest value is a 128-bit tagged value ([`value::Val128`]); guest references are
//! bounds-checked registry indices, never host addresses ([`registry`]), so host memory safety
//! holds even for adversarial bytecode (§4.3). Mutable per-object metadata is separated into a
//! control block so pointer copies never tear (§4.4). These layouts and seams are pinned on day
//! one because they thread through every slot access, field store, and allocation — retrofitting
//! them later would rewrite the interpreter (the "code-layering bug" hazard this crate averts).
//!
//! Per STD-CODE-2, `unsafe` is confined to this crate and `rjava-std` primitives; the atomics are
//! provided safely by `portable-atomic` (which also supplies the ARM64 128-bit fallback, §12.6).

pub mod diff;
pub mod env;
pub mod ids;
pub mod ir;
pub mod lock;
pub mod native;
pub mod registry;
pub mod value;

pub use diff::{chains_conflict, DiffNode, EnvSnapshot, EscapeState, FieldKey, ForkRegistry};
pub use env::{Env, LogicalFrame, MAX_SLOTS};
pub use ids::{BlockId, ClassId, InternedName, LoaderId, RefIndex, RegistryKey, SlotId, VtId};
pub use ir::{
    Block, Effect, ExcRegion, IntCond, L2Op, Method, Node, Op, OpStream, Phi, Terminator, ValId,
    ACCEL_BLOB_BASE, ACCEL_INTERPRET, ACCEL_JIT_PENDING,
};
pub use lock::{Monitor, U16Lock, U32Lock};
pub use native::{BuiltinClass, BuiltinMethod, NativeEnv, NativeError, NativeFn};
pub use registry::{ControlBlock, RefCounts, RegistryVec};
pub use value::{Tag, Val128, PAYLOAD_BITS};
