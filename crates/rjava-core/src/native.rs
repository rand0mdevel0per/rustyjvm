//! The native-method interface (RJVM-SPEC-001 §16.1, §18).
//!
//! `rustystd` implements `java.*` in Rust, and it depends on the interpreter for reentry (§3.2) —
//! so the interpreter cannot depend on it in turn. The seam that breaks the cycle lives here: this
//! module defines *what a native method may do* as a trait plus a plain function-pointer type, so
//! `rjava-std` writes methods against the trait and `rjava-interp` implements it. Builtin classes
//! are injected as data ([`BuiltinClass`]), never linked in.

use crate::value::Val128;

/// A failure inside a native method. The interpreter maps these onto the JVMS-specified
/// exception/error types (§22.1, STD-CODE-4); increment 8 turns them into real Java throwables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeError {
    OutOfMemory,
    NullPointer,
    /// A value of the wrong shape reached a native method — impossible for verified code (§4.3).
    BadValue,
}

/// The capabilities a native method has over the running VM.
///
/// Deliberately narrow: a native method can allocate and read builtin objects, touch fields by
/// absolute slot, and write to the guest's standard output. It never sees a host pointer to guest
/// state (STD-CODE-1) and cannot reach the registry or the diff machinery.
pub trait NativeEnv {
    /// Allocate a guest `java.lang.String` holding `s`.
    fn new_string(&mut self, s: &str) -> Result<Val128, NativeError>;
    /// The text of a guest `String` reference, or `None` if it is not one.
    fn string_text(&self, v: Val128) -> Option<String>;
    /// Write to the guest's standard output (an `Effect::Extern` operation, §10.6).
    fn print(&mut self, s: &str);
    /// Read an instance field by absolute slot.
    fn get_field(&self, obj: Val128, slot: u16) -> Result<Val128, NativeError>;
    /// Write an instance field by absolute slot.
    fn set_field(&mut self, obj: Val128, slot: u16, v: Val128) -> Result<(), NativeError>;
    /// The runtime class name of a reference (for `getClass`/`toString`).
    fn class_name_of(&self, v: Val128) -> Option<String>;
}

/// A native method body: `args` are the arguments in order, with the receiver first for an instance
/// method. Returns `None` for a `void` method.
pub type NativeFn = fn(&mut dyn NativeEnv, &[Val128]) -> Result<Option<Val128>, NativeError>;

/// One method of a builtin class.
pub struct BuiltinMethod {
    pub name: &'static str,
    pub descriptor: &'static str,
    /// `true` for `static`; instance methods are virtually dispatched like any other (§13.4).
    pub is_static: bool,
    pub body: NativeFn,
}

/// A class provided by `rustystd` rather than loaded from a class file (§17: the genesis classes
/// have no bytecode to load). Registered into the class registry so guest code resolves it exactly
/// like any other class.
pub struct BuiltinClass {
    pub name: &'static str,
    /// `None` for `java/lang/Object`, whose superclass is `null` in a conformant JVM (§17.2).
    pub super_name: Option<&'static str>,
    /// Instance fields, in declaration order: `(name, descriptor)`.
    pub fields: &'static [(&'static str, &'static str)],
    /// Static fields, in declaration order: `(name, descriptor)`.
    pub statics: &'static [(&'static str, &'static str)],
    pub methods: Vec<BuiltinMethod>,
}
