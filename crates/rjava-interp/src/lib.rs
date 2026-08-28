//! rjava-interp — the L3 register interpreter (RJVM-SPEC-001 §10.1). Increment 1 executes an L1
//! SSA method directly over a single [`Env`] (register file): each `Node` reads its `ins` slots,
//! computes, and writes its own slot; `Terminator`s drive control flow. This is the correct
//! baseline executor per P-1 — no speculation, no fork, no heap yet. The `match` on `Op` is the
//! computed-jump dispatch; the diff/fork machinery and the `L2Op.accel` JIT seam attach in
//! increments 4+ without changing this loop.
//!
//! IEEE-754 is strict (§13.6): Rust's wrapping integer ops and saturating float→int `as` casts
//! match the JVM's `iadd`/`ldiv`/`f2i`/… semantics exactly, which is what makes the differential
//! results bit-identical to Corretto.

use rjava_core::{
    BlockId, ClassId, Effect, Env, EscapeState, IntCond, LogicalFrame, Op, RefIndex, SlotId, Tag,
    Terminator, Val128,
};
use rjava_gc::Heap;
use rjava_ir::BuiltMethod;
use rjava_loader::{ClassRegistry, InitState, MethodBody, Resolved};

/// A runtime failure. Increment 1 cannot yet raise Java exceptions (those arrive in increment 8);
/// conditions like division by zero surface as errors here for now.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExecError {
    #[error("division by zero")]
    DivByZero,
    #[error("value/type error during execution")]
    BadValue,
    #[error("operation not supported by the increment-1 interpreter")]
    UnsupportedOp,
    #[error("exception thrown (implemented in increment 8)")]
    Thrown,
    #[error("execution step limit exceeded")]
    StepLimit,
    #[error("control transfer to a nonexistent block")]
    BadBlock,
    #[error("argument count mismatch")]
    BadArgs,
    #[error("phi has no operand for the incoming control-flow edge")]
    BadPhi,
    #[error("call stack depth exceeded (StackOverflowError)")]
    StackOverflow,
    #[error("no such method, or the callee failed to build")]
    NoMethod,
    #[error("out of memory (allocation failed, §22.1)")]
    OutOfMemory,
    #[error("null pointer (NullPointerException, implemented in increment 8)")]
    NullPointer,
    #[error("a symbolic reference could not be resolved (NoClassDefFoundError)")]
    Unresolved,
    #[error("class initialisation failed (NoClassDefFoundError)")]
    ClassInitFailed,
    #[error("bad cast (ClassCastException, implemented in increment 8)")]
    ClassCast,
    #[error("a native method failed: {0:?}")]
    Native(rjava_core::NativeError),
}

impl From<rjava_core::NativeError> for ExecError {
    fn from(e: rjava_core::NativeError) -> Self {
        match e {
            rjava_core::NativeError::OutOfMemory => ExecError::OutOfMemory,
            rjava_core::NativeError::NullPointer => ExecError::NullPointer,
            rjava_core::NativeError::BadValue => ExecError::BadValue,
        }
    }
}

/// Default guest call-stack limit, in frames.
///
/// The interpreter currently recurses on the **host** stack for each Java call, so this bound must
/// keep the deepest guest recursion inside the host thread's stack. Measured worst case (a debug
/// build on a default 2 MiB thread stack) is ~4 KiB of host stack per frame: 400 frames survive and
/// 600 overflow, so the default leaves roughly a 1.5× margin. Without such a bound, deep guest
/// recursion aborts the process with a host stack overflow instead of raising a Java
/// `StackOverflowError` — silently trading a catchable error for a crash.
///
/// An embedder that runs guest code on a larger stack can raise this via
/// [`Program::with_max_call_depth`]. The real fix is an explicit frame stack (an iterative
/// interpreter), which increment 8 needs anyway for stack traces (§20.7).
pub const DEFAULT_MAX_CALL_DEPTH: usize = 256;

/// A loaded program: a class registry plus the entry class used by the name-based helpers.
///
/// Symbolic references in the IR are resolved against this registry at execution time (§8.3), and
/// virtual calls read the *receiver's* dispatch table (§13.4), so nothing about class identity or
/// dispatch is fixed at build time.
pub struct Program {
    registry: ClassRegistry,
    entry: ClassId,
    /// Guest call-stack limit (see [`DEFAULT_MAX_CALL_DEPTH`]).
    max_call_depth: usize,
    /// The interned string pool. Interning is observable — two `ldc` of the same literal must yield
    /// the same reference (§18.4) — and interned strings are GC roots (§6.3), so entries here are
    /// never released.
    interned: std::cell::RefCell<std::collections::HashMap<String, Val128>>,
    /// Where guest output goes. `None` writes through to the process's standard output, which is
    /// what a launcher wants; `Some(buf)` captures it so a differential test can compare the bytes
    /// against Corretto's.
    captured: std::cell::RefCell<Option<Vec<u8>>>,
    /// The object heap. It belongs to the VM, not to a call: interned strings and `System.out` are
    /// references *into* it that outlive any single invocation, so a per-call heap would leave them
    /// dangling the moment a second method ran.
    heap: std::cell::RefCell<Heap>,
}

impl Program {
    /// Build a single-class program (the increment 1–5 entry point).
    pub fn from_class(cf: &rjava_classfile::ClassFile) -> Program {
        let mut registry = ClassRegistry::new();
        install_builtins(&mut registry);
        let entry = registry.link_class(cf.clone());
        Program::with_registry(registry, entry)
    }

    fn with_registry(registry: ClassRegistry, entry: ClassId) -> Program {
        Program {
            registry,
            entry,
            max_call_depth: DEFAULT_MAX_CALL_DEPTH,
            interned: std::cell::RefCell::new(std::collections::HashMap::new()),
            captured: std::cell::RefCell::new(None),
            heap: std::cell::RefCell::new(Heap::new()),
        }
    }

    /// Cap the number of simultaneously-live objects, so a test can prove that reclamation actually
    /// happens rather than merely not overflowing (§22.1).
    pub fn with_heap_limit(self, objects: usize) -> Self {
        *self.heap.borrow_mut() = Heap::with_limit(objects);
        self
    }

    /// Number of live objects on this VM's heap.
    pub fn heap_live(&self) -> usize {
        self.heap.borrow().live()
    }

    /// Capture guest output instead of writing it through to standard output.
    pub fn capturing(self) -> Self {
        *self.captured.borrow_mut() = Some(Vec::new());
        self
    }

    /// Take everything the guest has printed since the last call (capturing programs only).
    pub fn take_output(&self) -> Vec<u8> {
        self.captured
            .borrow_mut()
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    /// Write guest output to wherever this program directs it.
    fn write_output(&self, s: &str) {
        let mut sink = self.captured.borrow_mut();
        match sink.as_mut() {
            Some(buf) => buf.extend_from_slice(s.as_bytes()),
            None => {
                use std::io::Write;
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(s.as_bytes());
                let _ = out.flush();
            }
        }
    }

    /// Load and link every class in a classpath directory, with `entry_class` as the default class
    /// for the name-based helpers.
    pub fn from_classpath(
        dir: &std::path::Path,
        entry_class: &str,
    ) -> Result<Program, rjava_loader::LoaderError> {
        let mut registry = ClassRegistry::new();
        install_builtins(&mut registry);
        registry.load_dir(dir)?;
        let entry = registry
            .by_name(entry_class)
            .ok_or_else(|| rjava_loader::LoaderError::NotFound(entry_class.to_string()))?;
        Ok(Program::with_registry(registry, entry))
    }

    pub fn registry(&self) -> &ClassRegistry {
        &self.registry
    }

    /// Raise (or lower) the guest call-stack limit — only sound if guest code runs on a host thread
    /// whose stack can hold that many frames (see [`DEFAULT_MAX_CALL_DEPTH`]).
    pub fn with_max_call_depth(mut self, frames: usize) -> Self {
        self.max_call_depth = frames;
        self
    }

    /// Invoke a method of `class` by its index in that class's method table.
    pub fn call(
        &self,
        class: ClassId,
        index: u16,
        args: &[Val128],
        depth: usize,
        heap: &mut Heap,
    ) -> Result<Option<Val128>, ExecError> {
        let body = self
            .registry
            .get(class)
            .and_then(|k| k.methods.get(index as usize))
            .ok_or(ExecError::NoMethod)?;
        match body {
            MethodBody::Bytecode(built) => run(
                self,
                class,
                built,
                args,
                depth,
                LogicalFrame(index as u32),
                heap,
            ),
            // A `rustystd` method is an `Effect::Extern` boundary, so it runs directly (§10.6,
            // §16.1) rather than through the diff machinery.
            MethodBody::Native(f) => {
                let r = {
                    let mut ctx = Ctx {
                        program: self,
                        heap,
                    };
                    f(&mut ctx, args)?
                };
                // A native method hands its result over the same way a bytecode frame does: with an
                // in-transit reference the caller drops once one of its slots owns it (§5.5).
                // Without it the caller's acquire/release pair nets to zero and a freshly created
                // String is reclaimed the instant it is stored.
                if let Some(v) = r {
                    if v.tag().is_ref() {
                        heap.add_rc(v.ref_index(), 1);
                    }
                }
                Ok(r)
            }
            MethodBody::Absent => Err(ExecError::NoMethod),
        }
    }

    /// The entry class's index for a method, by name and descriptor.
    pub fn method_index(&self, name: &str, desc: &str) -> Option<u16> {
        self.registry.get(self.entry)?.method_index(name, desc)
    }

    /// Invoke a method of the entry class by name + descriptor (the launcher/test entry point).
    pub fn call_named(
        &self,
        name: &str,
        desc: &str,
        args: &[Val128],
    ) -> Result<Option<Val128>, ExecError> {
        let idx = self.method_index(name, desc).ok_or(ExecError::NoMethod)?;
        let mut heap = self.heap.borrow_mut();
        self.init_system_streams(&mut heap)?;
        self.call(self.entry, idx, args, 0, &mut heap)
    }

    /// Allocate a `java.lang.String` holding `text`.
    fn alloc_string(&self, heap: &mut Heap, text: &str) -> Result<Val128, ExecError> {
        let cls = self
            .registry
            .by_name("java/lang/String")
            .ok_or(ExecError::Unresolved)?;
        let defaults = self
            .registry
            .get(cls)
            .ok_or(ExecError::Unresolved)?
            .field_defaults
            .clone();
        // A String is shared the moment it exists (it is handed to callers and interned), so it is
        // reference counted rather than scope-exclusive (§5.2).
        let r = heap
            .alloc(cls, EscapeState::S2, defaults)
            .ok_or(ExecError::OutOfMemory)?;
        heap.set_text(r, text);
        Ok(Val128::ptr(r))
    }

    /// The interned `String` for a literal: the same literal always yields the same reference
    /// (§18.4). Interned strings are roots, so their count is pinned above zero.
    fn intern(&self, heap: &mut Heap, text: &str) -> Result<Val128, ExecError> {
        if let Some(&v) = self.interned.borrow().get(text) {
            return Ok(v);
        }
        let v = self.alloc_string(heap, text)?;
        // Pin it: the pool holds a permanent reference, so it is never reclaimed.
        heap.add_rc(v.ref_index(), 1);
        self.interned.borrow_mut().insert(text.to_string(), v);
        Ok(v)
    }

    /// Create the `PrintStream` singletons and store them in `System.out` / `System.err`.
    fn init_system_streams(&self, heap: &mut Heap) -> Result<(), ExecError> {
        let sys = self
            .registry
            .by_name("java/lang/System")
            .ok_or(ExecError::Unresolved)?;
        let k = self.registry.get(sys).ok_or(ExecError::Unresolved)?;
        let ps = self
            .registry
            .by_name("java/io/PrintStream")
            .ok_or(ExecError::Unresolved)?;
        for field in ["out", "err"] {
            let Some(slot) = k.static_slot(field, "Ljava/io/PrintStream;") else {
                continue;
            };
            if k.statics.borrow()[slot].tag().is_ref() {
                continue; // already installed
            }
            let defaults = self
                .registry
                .get(ps)
                .ok_or(ExecError::Unresolved)?
                .field_defaults
                .clone();
            let r = heap
                .alloc(ps, EscapeState::S2, defaults)
                .ok_or(ExecError::OutOfMemory)?;
            heap.add_rc(r, 1); // a static field is a root-held reference
            k.statics.borrow_mut()[slot] = Val128::ptr(r);
        }
        Ok(())
    }

    /// Run `<clinit>` if this class has not been initialised yet (§8.5, JVMS §5.5).
    ///
    /// Superclasses initialise first; a vt already running this class's `<clinit>` passes straight
    /// through instead of deadlocking; and a `<clinit>` that throws marks the class `Failed` so
    /// later uses raise `NoClassDefFoundError` rather than silently seeing half-set statics.
    fn ensure_init(&self, class: ClassId, depth: usize, heap: &mut Heap) -> Result<(), ExecError> {
        let Some(k) = self.registry.get(class) else {
            return Err(ExecError::NoMethod);
        };
        match k.init_state() {
            InitState::Initialized => return Ok(()),
            InitState::Failed => return Err(ExecError::ClassInitFailed),
            // Same vt re-entering its own <clinit>: JVMS §5.5 pass-through.
            InitState::Initializing(_) => return Ok(()),
            InitState::Loaded => {}
        }
        k.set_init_state(InitState::Initializing(0));
        if let Some(sup) = k.super_id {
            self.ensure_init(sup, depth, heap)?;
        }
        if let Some(idx) = k.method_index("<clinit>", "()V") {
            match self.call(class, idx, &[], depth + 1, heap) {
                Ok(_) => {}
                Err(e) => {
                    k.set_init_state(InitState::Failed);
                    return Err(e);
                }
            }
        }
        k.set_init_state(InitState::Initialized);
        Ok(())
    }
}

/// Register `rustystd`'s builtin classes and wire up `System.out` (§18, §17).
///
/// The builtins are handed over as data, so the interpreter never links against `rjava-std` — the
/// dependency direction of §3.2 stays intact.
fn install_builtins(registry: &mut ClassRegistry) {
    for b in rjava_std::builtins() {
        registry.register_builtin(b);
    }
}

/// The [`NativeEnv`] a native method sees: the program (for class lookup and interning) and the
/// heap. Deliberately narrow — a native method cannot reach the registry or the diff machinery.
struct Ctx<'a> {
    program: &'a Program,
    heap: &'a mut Heap,
}

impl rjava_core::NativeEnv for Ctx<'_> {
    fn new_string(&mut self, s: &str) -> Result<Val128, rjava_core::NativeError> {
        self.program
            .alloc_string(self.heap, s)
            .map_err(|_| rjava_core::NativeError::OutOfMemory)
    }

    fn string_text(&self, v: Val128) -> Option<String> {
        if !v.tag().is_ref() {
            return None;
        }
        let obj = self.heap.get(v.ref_index())?;
        // Only a real String carries text, so this cannot be spoofed by another class.
        if self
            .program
            .registry
            .get(obj.class)
            .map(|k| k.name.as_str())
            != Some("java/lang/String")
        {
            return None;
        }
        self.heap.text(v.ref_index()).map(str::to_string)
    }

    fn print(&mut self, s: &str) {
        // Printing is `Effect::Extern`: it lands in program order and is never speculated (§10.6).
        self.program.write_output(s);
    }

    fn get_field(&self, obj: Val128, slot: u16) -> Result<Val128, rjava_core::NativeError> {
        if obj.tag() == Tag::Null {
            return Err(rjava_core::NativeError::NullPointer);
        }
        self.heap
            .get_field(obj.ref_index(), slot as usize)
            .ok_or(rjava_core::NativeError::BadValue)
    }

    fn set_field(
        &mut self,
        obj: Val128,
        slot: u16,
        v: Val128,
    ) -> Result<(), rjava_core::NativeError> {
        if obj.tag() == Tag::Null {
            return Err(rjava_core::NativeError::NullPointer);
        }
        if self.heap.set_field(obj.ref_index(), slot as usize, v) {
            Ok(())
        } else {
            Err(rjava_core::NativeError::BadValue)
        }
    }

    fn class_name_of(&self, v: Val128) -> Option<String> {
        let obj = self.heap.get(v.ref_index())?;
        self.program.registry.get(obj.class).map(|k| k.name.clone())
    }
}

/// Land a chain: apply its recorded reference-count deltas to the heap in program order, then
/// commit its slot writes (§5.5, §10.5). `buf` is reused across landings to avoid allocating.
#[inline]
fn land_chain(env: &mut Env, heap: &mut Heap, buf: &mut Vec<(RefIndex, i32)>) {
    env.drain_ref_deltas(buf);
    if !buf.is_empty() {
        heap.apply_rc_deltas(buf);
    }
    env.land();
}

/// Leave a frame: land whatever is still pending, then release the references its slots held.
///
/// The returned reference is handed to the caller as an *in-transit* reference (+1) inside the same
/// batch that releases the slots, so it cannot be reclaimed in between; the caller drops that
/// in-transit reference once a slot of its own owns it. Scope-exclusive (S1) objects were never
/// counted at all — they are reclaimed here by RAII (§5.1), which also releases anything their
/// fields referenced.
fn leave_frame(
    env: &mut Env,
    heap: &mut Heap,
    buf: &mut Vec<(RefIndex, i32)>,
    n_slots: usize,
    returned: Option<Val128>,
) {
    land_chain(env, heap, buf);

    buf.clear();
    if let Some(v) = returned {
        if v.tag().is_ref() {
            buf.push((v.ref_index(), 1)); // in-transit reference for the caller
        }
    }
    for i in 0..n_slots {
        let v = env.read_slot(SlotId(i as u16));
        if v.tag() == Tag::Ptr {
            buf.push((v.ref_index(), -1));
        }
    }
    if !buf.is_empty() {
        heap.apply_rc_deltas(buf);
    }
    // S1 handles die with the scope that owned them (§5.1). A handle may sit in several slots
    // (a φ merge); reclaiming is idempotent, so the repeat is harmless.
    for i in 0..n_slots {
        let v = env.read_slot(SlotId(i as u16));
        if v.tag() == Tag::Handle {
            heap.free(v.ref_index());
        }
    }
}

/// Execute a built method over a fresh Env register file, tearing the frame down on **every** exit.
///
/// `depth`/`logical` thread the call stack (StackOverflow seam + logical frames, §20.7). A frame
/// that leaves abruptly must release what it owned just as a returning one does — otherwise a throw
/// would leak its S1 handles and S2 references — so the teardown lives here, around the body,
/// rather than at each `return`.
#[allow(clippy::too_many_arguments)]
fn run(
    program: &Program,
    owner: ClassId,
    built: &BuiltMethod,
    args: &[Val128],
    depth: usize,
    logical: LogicalFrame,
    heap: &mut Heap,
) -> Result<Option<Val128>, ExecError> {
    if depth > program.max_call_depth {
        return Err(ExecError::StackOverflow);
    }
    if args.len() != built.arg_vals.len() {
        return Err(ExecError::BadArgs);
    }
    let n_slots = built.n_slots.max(1);
    let mut env = Env::new(n_slots, logical);
    for (slot, &value) in built.arg_vals.iter().zip(args) {
        env.write_slot(SlotId(slot.offset), value);
    }
    let mut rc_buf: Vec<(RefIndex, i32)> = Vec::new();

    let result = run_body(program, owner, built, depth, heap, &mut env, &mut rc_buf);

    match result {
        Ok(rv) => leave_frame(&mut env, heap, &mut rc_buf, n_slots, rv),
        Err(_) => {
            // An abrupt exit still owns whatever the frame allocated, so it is torn down exactly
            // like a return, minus a value to hand over. The pending diff is landed rather than
            // discarded: the frame is being destroyed, so its slots are unobservable either way,
            // but teardown needs them to see which references to release — discarding first would
            // orphan anything allocated in the chain that threw. (§20.5's program-order truncation
            // is a property of a *handler* resuming, which arrives with exceptions in increment 8;
            // it is not what happens when a frame is destroyed outright, §20.4.)
            leave_frame(&mut env, heap, &mut rc_buf, n_slots, None);
        }
    }
    result
}

/// The interpreter loop. Every exit — normal or abrupt — is torn down by [`run`].
#[allow(clippy::too_many_arguments)]
fn run_body(
    program: &Program,
    owner: ClassId,
    built: &BuiltMethod,
    depth: usize,
    heap: &mut Heap,
    env: &mut Env,
    rc_buf: &mut Vec<(RefIndex, i32)>,
) -> Result<Option<Val128>, ExecError> {
    let mut cur = built.method.entry;
    let mut prev: Option<BlockId> = None;
    let mut steps: u64 = 0;
    loop {
        steps += 1;
        if steps > 100_000_000 {
            return Err(ExecError::StepLimit);
        }
        let block = built
            .method
            .blocks
            .get(cur.0 as usize)
            .ok_or(ExecError::BadBlock)?;

        // φ resolution: read every source for the edge we arrived on, then write all φ slots
        // (parallel-copy semantics so self-referential loop φ behave correctly).
        if let Some(p) = prev {
            if !block.phis.is_empty() {
                let mut writes: smallvec::SmallVec<[(u16, Val128); 4]> = smallvec::SmallVec::new();
                for phi in &block.phis {
                    let (_, v) = phi
                        .sources
                        .iter()
                        .find(|(pb, _)| *pb == p)
                        .ok_or(ExecError::BadPhi)?;
                    writes.push((phi.slot.offset, env.read_slot(SlotId(v.offset))));
                }
                for (slot, val) in writes {
                    env.write_slot(SlotId(slot), val);
                }
            }
        }

        for node in &block.nodes {
            // `Effect::Extern` is a fence: it is never speculated and its predecessors land in
            // program order before it runs (§10.6). Calls carry this effect.
            if node.effect == Effect::Extern {
                land_chain(env, heap, rc_buf);
            }
            match node.op {
                // Calls (need the Program + recursion + shared heap).
                // ---- calls: symbolic targets resolved through the registry (§8.3) ----
                Op::InvokeStatic(cp) | Op::InvokeSpecial(cp) | Op::InvokeVirtual(cp) => {
                    let mut argv: smallvec::SmallVec<[Val128; 4]> = smallvec::SmallVec::new();
                    for v in &node.ins {
                        argv.push(env.read_slot(SlotId(v.offset)));
                    }
                    let target = match node.op {
                        Op::InvokeVirtual(_) => {
                            // Dispatch on the RECEIVER's runtime class, through its dispatch table
                            // (§13.4) — never on the class named at the call site.
                            let recv = *argv.first().ok_or(ExecError::BadValue)?;
                            if recv.tag() == Tag::Null {
                                return Err(ExecError::NullPointer);
                            }
                            let Resolved::VirtualSlot { slot } = program
                                .registry
                                .resolve_virtual(owner, cp)
                                .ok_or(ExecError::Unresolved)?
                            else {
                                return Err(ExecError::Unresolved);
                            };
                            let rc = heap.get(recv.ref_index()).ok_or(ExecError::BadValue)?.class;
                            let entry = *program
                                .registry
                                .get(rc)
                                .and_then(|k| k.vtable.get(slot))
                                .ok_or(ExecError::Unresolved)?;
                            Some((entry.class, entry.method))
                        }
                        _ => match program
                            .registry
                            .resolve_method(owner, cp)
                            .ok_or(ExecError::Unresolved)?
                        {
                            Resolved::Method { class, method } => Some((class, method)),
                            // Object.<init> — the one absent target with no observable effect.
                            Resolved::NoOp => None,
                            _ => return Err(ExecError::Unresolved),
                        },
                    };
                    if let Some((cls, midx)) = target {
                        // A static call is an active use of its class (§8.5).
                        if matches!(node.op, Op::InvokeStatic(_)) {
                            program.ensure_init(cls, depth, heap)?;
                        }
                        if let Some(r) = program.call(cls, midx, &argv, depth + 1, heap)? {
                            env.write_slot(SlotId(node.id.offset), r);
                            // The callee handed over an in-transit reference; now that a slot of
                            // ours owns it, drop that extra reference (both deltas land together).
                            if r.tag().is_ref() {
                                env.record_ref_delta(r.ref_index(), -1);
                            }
                        }
                    }
                }
                // ---- heap ----
                Op::New { class_cp, escape } => {
                    let cls = program
                        .registry
                        .resolve_class(owner, class_cp)
                        .ok_or(ExecError::Unresolved)?;
                    program.ensure_init(cls, depth, heap)?;
                    let defaults = program
                        .registry
                        .get(cls)
                        .ok_or(ExecError::Unresolved)?
                        .field_defaults
                        .clone();
                    let r = heap
                        .alloc(cls, escape, defaults)
                        .ok_or(ExecError::OutOfMemory)?;
                    let v = if escape == EscapeState::S1 {
                        Val128::handle(r)
                    } else {
                        Val128::ptr(r)
                    };
                    env.write_slot(SlotId(node.id.offset), v);
                }
                Op::GetField(cp) => {
                    let obj =
                        env.read_slot(SlotId(node.ins.first().ok_or(ExecError::BadValue)?.offset));
                    if obj.tag() == Tag::Null {
                        return Err(ExecError::NullPointer);
                    }
                    let Resolved::InstanceField { slot, .. } = program
                        .registry
                        .resolve_field(owner, cp)
                        .ok_or(ExecError::Unresolved)?
                    else {
                        return Err(ExecError::Unresolved);
                    };
                    let v = heap
                        .get_field(obj.ref_index(), slot as usize)
                        .ok_or(ExecError::BadValue)?;
                    env.write_slot(SlotId(node.id.offset), v);
                }
                Op::PutField(cp) => {
                    let obj =
                        env.read_slot(SlotId(node.ins.first().ok_or(ExecError::BadValue)?.offset));
                    let val =
                        env.read_slot(SlotId(node.ins.get(1).ok_or(ExecError::BadValue)?.offset));
                    if obj.tag() == Tag::Null {
                        return Err(ExecError::NullPointer);
                    }
                    let Resolved::InstanceField { slot, .. } = program
                        .registry
                        .resolve_field(owner, cp)
                        .ok_or(ExecError::Unresolved)?
                    else {
                        return Err(ExecError::Unresolved);
                    };
                    // A field is a strong reference too: the overwritten referent loses one and the
                    // stored one gains it. Recorded now, applied when the chain lands (§5.5).
                    let old = heap
                        .get_field(obj.ref_index(), slot as usize)
                        .ok_or(ExecError::BadValue)?;
                    if !heap.set_field(obj.ref_index(), slot as usize, val) {
                        return Err(ExecError::BadValue);
                    }
                    if val.tag().is_ref() {
                        env.record_ref_delta(val.ref_index(), 1);
                    }
                    if old.tag().is_ref() {
                        env.record_ref_delta(old.ref_index(), -1);
                    }
                }
                Op::LoadString(cp) => {
                    let text = program
                        .registry
                        .get(owner)
                        .and_then(|k| k.constant_pool())
                        .and_then(|cp_pool| match cp_pool.get(cp) {
                            Some(rjava_classfile::Constant::String { string_index }) => {
                                cp_pool.utf8(*string_index)
                            }
                            _ => None,
                        })
                        .ok_or(ExecError::Unresolved)?
                        .to_string();
                    let v = program.intern(heap, &text)?;
                    env.write_slot(SlotId(node.id.offset), v);
                }
                Op::GetStatic(cp) | Op::PutStatic(cp) => {
                    let Resolved::StaticField { class, slot } = program
                        .registry
                        .resolve_field(owner, cp)
                        .ok_or(ExecError::Unresolved)?
                    else {
                        return Err(ExecError::Unresolved);
                    };
                    // Touching a static field is an active use of its declaring class (§8.5).
                    program.ensure_init(class, depth, heap)?;
                    let k = program.registry.get(class).ok_or(ExecError::Unresolved)?;
                    if let Op::GetStatic(_) = node.op {
                        let v = *k.statics.borrow().get(slot).ok_or(ExecError::BadValue)?;
                        env.write_slot(SlotId(node.id.offset), v);
                    } else {
                        let val = env
                            .read_slot(SlotId(node.ins.first().ok_or(ExecError::BadValue)?.offset));
                        let mut statics = k.statics.borrow_mut();
                        let cell = statics.get_mut(slot).ok_or(ExecError::BadValue)?;
                        let old = *cell;
                        *cell = val;
                        drop(statics);
                        // A static field is a root-held strong reference, counted like any other.
                        if val.tag().is_ref() {
                            env.record_ref_delta(val.ref_index(), 1);
                        }
                        if old.tag().is_ref() {
                            env.record_ref_delta(old.ref_index(), -1);
                        }
                    }
                }
                Op::InstanceOf(cp) | Op::CheckCast(cp) => {
                    let obj =
                        env.read_slot(SlotId(node.ins.first().ok_or(ExecError::BadValue)?.offset));
                    let is_instance = if obj.tag() == Tag::Null {
                        false // `null instanceof T` is false; a cast of null always succeeds
                    } else {
                        let cls = program
                            .registry
                            .resolve_class(owner, cp)
                            .ok_or(ExecError::Unresolved)?;
                        let rc = heap.get(obj.ref_index()).ok_or(ExecError::BadValue)?.class;
                        program.registry.is_subclass_of(rc, cls)
                    };
                    if let Op::InstanceOf(_) = node.op {
                        env.write_slot(
                            SlotId(node.id.offset),
                            Val128::from_i32(is_instance as i32),
                        );
                    } else {
                        if !is_instance && obj.tag() != Tag::Null {
                            return Err(ExecError::ClassCast);
                        }
                        env.write_slot(SlotId(node.id.offset), obj);
                    }
                }
                // Pure computations.
                _ => {
                    let mut inputs: smallvec::SmallVec<[Val128; 3]> = smallvec::SmallVec::new();
                    for v in &node.ins {
                        inputs.push(env.read_slot(SlotId(v.offset)));
                    }
                    let result = eval(node.op, node.ty, &inputs)?;
                    env.write_slot(SlotId(node.id.offset), result);
                }
            }
        }

        match &block.term {
            Terminator::Return(Some(v)) => return Ok(Some(env.read_slot(SlotId(v.offset)))),
            Terminator::Return(None) => return Ok(None),
            Terminator::Goto(b) => {
                prev = Some(cur);
                cur = *b;
            }
            Terminator::CondBranch {
                cond,
                taken,
                not_taken,
            } => {
                let c = env.read_slot(SlotId(cond.offset)).as_i32();
                prev = Some(cur);
                cur = if c != 0 { *taken } else { *not_taken };
            }
            Terminator::Throw(_) => return Err(ExecError::Thrown),
        }
        // End of chain: land this block's diff into the environment in program order (§10.5).
        // Landing incrementally — rather than batching at scope exit — is what keeps intra-vt
        // execution as-if-serial and makes the increment-8 po-truncation on a caught throw correct.
        land_chain(env, heap, rc_buf);
    }
}

/// Evaluate a single SSA node.
fn eval(op: Op, ty: Tag, ins: &[Val128]) -> Result<Val128, ExecError> {
    Ok(match op {
        Op::Const(v) => v,
        Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Rem => {
            let a = *ins.first().ok_or(ExecError::BadValue)?;
            let b = *ins.get(1).ok_or(ExecError::BadValue)?;
            binary(op, ty, a, b)?
        }
        Op::Neg => {
            let a = *ins.first().ok_or(ExecError::BadValue)?;
            match ty {
                Tag::I32 => Val128::from_i32(a.as_i32().wrapping_neg()),
                Tag::I64 => Val128::from_i64(a.as_i64().wrapping_neg()),
                Tag::F32 => Val128::from_f32(-a.as_f32()),
                Tag::F64 => Val128::from_f64(-a.as_f64()),
                _ => return Err(ExecError::UnsupportedOp),
            }
        }
        Op::Convert => convert(*ins.first().ok_or(ExecError::BadValue)?, ty)?,
        Op::Cmp { nan_greater } => {
            let a = *ins.first().ok_or(ExecError::BadValue)?;
            let b = *ins.get(1).ok_or(ExecError::BadValue)?;
            cmp3(a, b, nan_greater)?
        }
        Op::TestZero(cond) => {
            let v = ins.first().ok_or(ExecError::BadValue)?.as_i32();
            let taken = match cond {
                IntCond::Eq => v == 0,
                IntCond::Ne => v != 0,
                IntCond::Lt => v < 0,
                IntCond::Ge => v >= 0,
                IntCond::Gt => v > 0,
                IntCond::Le => v <= 0,
            };
            Val128::from_i32(taken as i32)
        }
        Op::TestNull { expect_null } => {
            let is_null = ins.first().ok_or(ExecError::BadValue)?.tag() == Tag::Null;
            Val128::from_i32((is_null == expect_null) as i32)
        }
        Op::RefEq { expect_same } => {
            let a = *ins.first().ok_or(ExecError::BadValue)?;
            let b = *ins.get(1).ok_or(ExecError::BadValue)?;
            // Identity, not equality: two references name the same object iff they name the same
            // registry entry, and `null == null` holds.
            let same = match (a.tag(), b.tag()) {
                (Tag::Null, Tag::Null) => true,
                (x, y) if x.is_ref() && y.is_ref() => a.ref_index() == b.ref_index(),
                _ => false,
            };
            Val128::from_i32((same == expect_same) as i32)
        }
        Op::ICmp(cond) => {
            let a = ins.first().ok_or(ExecError::BadValue)?.as_i32();
            let b = ins.get(1).ok_or(ExecError::BadValue)?.as_i32();
            let taken = match cond {
                IntCond::Eq => a == b,
                IntCond::Ne => a != b,
                IntCond::Lt => a < b,
                IntCond::Ge => a >= b,
                IntCond::Gt => a > b,
                IntCond::Le => a <= b,
            };
            Val128::from_i32(taken as i32)
        }
        Op::And => {
            let a = *ins.first().ok_or(ExecError::BadValue)?;
            let b = *ins.get(1).ok_or(ExecError::BadValue)?;
            match ty {
                Tag::I32 => Val128::from_i32(a.as_i32() & b.as_i32()),
                Tag::I64 => Val128::from_i64(a.as_i64() & b.as_i64()),
                _ => return Err(ExecError::UnsupportedOp),
            }
        }
        // Calls, heap and class ops are dispatched in `run` (they need the registry and heap).
        Op::InvokeStatic(_)
        | Op::InvokeSpecial(_)
        | Op::InvokeVirtual(_)
        | Op::New { .. }
        | Op::GetField(_)
        | Op::PutField(_)
        | Op::GetStatic(_)
        | Op::PutStatic(_)
        | Op::InstanceOf(_)
        | Op::CheckCast(_)
        | Op::LoadString(_) => return Err(ExecError::UnsupportedOp),
    })
}

/// Typed binary arithmetic (add/sub/mul/div/rem). Integer div/rem trap on a zero divisor and wrap
/// on `MIN / -1`; floats follow strict IEEE-754.
fn binary(op: Op, ty: Tag, a: Val128, b: Val128) -> Result<Val128, ExecError> {
    Ok(match ty {
        Tag::I32 => {
            let (x, y) = (a.as_i32(), b.as_i32());
            Val128::from_i32(match op {
                Op::Add => x.wrapping_add(y),
                Op::Sub => x.wrapping_sub(y),
                Op::Mul => x.wrapping_mul(y),
                Op::Div => {
                    if y == 0 {
                        return Err(ExecError::DivByZero);
                    }
                    x.wrapping_div(y)
                }
                Op::Rem => {
                    if y == 0 {
                        return Err(ExecError::DivByZero);
                    }
                    x.wrapping_rem(y)
                }
                _ => return Err(ExecError::UnsupportedOp),
            })
        }
        Tag::I64 => {
            let (x, y) = (a.as_i64(), b.as_i64());
            Val128::from_i64(match op {
                Op::Add => x.wrapping_add(y),
                Op::Sub => x.wrapping_sub(y),
                Op::Mul => x.wrapping_mul(y),
                Op::Div => {
                    if y == 0 {
                        return Err(ExecError::DivByZero);
                    }
                    x.wrapping_div(y)
                }
                Op::Rem => {
                    if y == 0 {
                        return Err(ExecError::DivByZero);
                    }
                    x.wrapping_rem(y)
                }
                _ => return Err(ExecError::UnsupportedOp),
            })
        }
        Tag::F32 => {
            let (x, y) = (a.as_f32(), b.as_f32());
            Val128::from_f32(match op {
                Op::Add => x + y,
                Op::Sub => x - y,
                Op::Mul => x * y,
                Op::Div => x / y,
                Op::Rem => x % y,
                _ => return Err(ExecError::UnsupportedOp),
            })
        }
        Tag::F64 => {
            let (x, y) = (a.as_f64(), b.as_f64());
            Val128::from_f64(match op {
                Op::Add => x + y,
                Op::Sub => x - y,
                Op::Mul => x * y,
                Op::Div => x / y,
                Op::Rem => x % y,
                _ => return Err(ExecError::UnsupportedOp),
            })
        }
        _ => return Err(ExecError::UnsupportedOp),
    })
}

/// Numeric conversion driven by the source value's tag and the target type. Rust's `as` casts
/// match the JVM: int->float rounds to nearest; float->int saturates (NaN->0, out-of-range clamps),
/// exactly the `f2i`/`f2l`/`d2i`/`d2l` semantics.
fn convert(a: Val128, ty: Tag) -> Result<Val128, ExecError> {
    use Tag::{F32, F64, I32, I64};
    Ok(match (a.tag(), ty) {
        (I32, I64) => Val128::from_i64(a.as_i32() as i64),
        (I32, F32) => Val128::from_f32(a.as_i32() as f32),
        (I32, F64) => Val128::from_f64(a.as_i32() as f64),
        (I64, I32) => Val128::from_i32(a.as_i64() as i32),
        (I64, F32) => Val128::from_f32(a.as_i64() as f32),
        (I64, F64) => Val128::from_f64(a.as_i64() as f64),
        (F32, I32) => Val128::from_i32(a.as_f32() as i32),
        (F32, I64) => Val128::from_i64(a.as_f32() as i64),
        (F32, F64) => Val128::from_f64(a.as_f32() as f64),
        (F64, I32) => Val128::from_i32(a.as_f64() as i32),
        (F64, I64) => Val128::from_i64(a.as_f64() as i64),
        (F64, F32) => Val128::from_f32(a.as_f64() as f32),
        _ => return Err(ExecError::UnsupportedOp),
    })
}

/// Three-way compare (`lcmp`/`fcmp<op>`): -1/0/1. On a float NaN, `fcmpg` yields 1 and `fcmpl`
/// yields -1 (`nan_greater` selects which).
fn cmp3(a: Val128, b: Val128, nan_greater: bool) -> Result<Val128, ExecError> {
    let r = match a.tag() {
        Tag::I64 => {
            let (x, y) = (a.as_i64(), b.as_i64());
            (x > y) as i32 - (x < y) as i32
        }
        Tag::F32 => float_cmp(a.as_f32() as f64, b.as_f32() as f64, nan_greater),
        Tag::F64 => float_cmp(a.as_f64(), b.as_f64(), nan_greater),
        _ => return Err(ExecError::UnsupportedOp),
    };
    Ok(Val128::from_i32(r))
}

fn float_cmp(x: f64, y: f64, nan_greater: bool) -> i32 {
    if x.is_nan() || y.is_nan() {
        if nan_greater {
            1
        } else {
            -1
        }
    } else if x > y {
        1
    } else if x < y {
        -1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile_slice() -> Option<Vec<u8>> {
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java/Slice.java");
        if !src.exists() {
            return None;
        }
        let out = std::env::temp_dir().join(format!("rjava-ix-{}", std::process::id()));
        std::fs::create_dir_all(&out).ok()?;
        let ok = std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&out)
            .arg(&src)
            .status()
            .ok()?
            .success();
        if !ok {
            return None;
        }
        std::fs::read(out.join("Slice.class")).ok()
    }

    #[test]
    fn executes_slice_arith_matches_hand_computed() {
        let Some(bytes) = compile_slice() else {
            eprintln!("skipping executes_slice_arith: javac unavailable");
            return;
        };
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let program = Program::from_class(&cf);

        let run = |a: i32, b: i32, c: i64, d: f32| -> i32 {
            let args = [
                Val128::from_i32(a),
                Val128::from_i32(b),
                Val128::from_i64(c),
                Val128::from_f32(d),
            ];
            program
                .call_named("arith", "(IIJF)I", &args)
                .unwrap()
                .unwrap()
                .as_i32()
        };

        // Hand-computed oracles across all three return paths of `arith`.
        assert_eq!(run(1, 2, 3, 4.0), -7); // else: -s
        assert_eq!(run(50, 3, 100, 200.0), -53); // first: (int)(t-(long)u)+s
        assert_eq!(run(2, 0, 0, 0.0), 0); // else-if: s*s (s==0)
    }

    fn tool_ok(name: &str) -> bool {
        std::process::Command::new(name)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// A diverse input set for `arith`: boundary values, NaN/Inf/subnormals, and deterministic
    /// pseudo-random tuples. `b == -1` is excluded so `a / (b + 1)` never divides by zero (integer
    /// division traps arrive in increment 8).
    fn gen_inputs() -> Vec<(i32, i32, i64, f32)> {
        let ints = [
            0i32,
            1,
            -1,
            2,
            -2,
            3,
            -3,
            7,
            -7,
            100,
            -100,
            46340,
            -46340,
            i32::MIN,
            i32::MAX,
            i32::MIN + 1,
            i32::MAX - 1,
        ];
        let longs = [
            0i64,
            1,
            -1,
            2,
            -2,
            100,
            -100,
            1 << 40,
            -(1 << 40),
            i64::MIN,
            i64::MAX,
        ];
        let floats = [
            0.0f32,
            -0.0,
            1.5,
            -3.25,
            100.5,
            999.9,
            1000.0,
            1e30,
            -1e30,
            0.1,
            f32::MIN_POSITIVE,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ];
        let mut out = Vec::new();
        for &a in &ints {
            for &b in &ints {
                if b == -1 {
                    continue;
                }
                out.push((a, b, 100, 200.0));
                out.push((a, b, i64::MAX, f32::NAN));
            }
        }
        for &c in &longs {
            for &d in &floats {
                out.push((50, 3, c, d));
                out.push((1, 2, c, d));
            }
        }
        // Deterministic LCG (no rand dependency); vary widely across the whole domain.
        let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            s
        };
        for _ in 0..400 {
            let a = next() as i32;
            let mut b = next() as i32;
            if b == -1 {
                b = 0;
            }
            let c = next() as i64;
            let d = f32::from_bits(next() as u32);
            out.push((a, b, c, d));
        }
        out
    }

    /// Run the Corretto oracle: reconstruct each float from its raw bits, invoke `Slice.arith`, and
    /// return one result per input line.
    fn run_oracle(dir: &std::path::Path, inputs: &[(i32, i32, i64, f32)]) -> Option<Vec<i32>> {
        use std::io::Write;
        let mut stdin_data = String::new();
        for &(a, b, c, d) in inputs {
            use std::fmt::Write as _;
            writeln!(stdin_data, "{} {} {} {}", a, b, c, d.to_bits() as i32).unwrap();
        }
        let mut child = std::process::Command::new("java")
            .arg("-cp")
            .arg(dir)
            .arg("Oracle")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .ok()?;
        child.stdin.take()?.write_all(stdin_data.as_bytes()).ok()?;
        let out = child.wait_with_output().ok()?;
        if !out.status.success() {
            eprintln!("oracle failed: {}", String::from_utf8_lossy(&out.stderr));
            return None;
        }
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().parse::<i32>().ok())
            .collect()
    }

    /// The increment-1 conformance gate (§P-5, §23.1): RustyJVM's `arith` result MUST equal
    /// Corretto 21's for every input. Skips when the JDK is absent (runs in the CI `differential`
    /// job, which provisions Corretto 21).
    #[test]
    fn differential_vs_corretto_21() {
        if !tool_ok("javac") || !tool_ok("java") {
            eprintln!("skipping differential_vs_corretto_21: JDK unavailable");
            return;
        }
        let slice_src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java/Slice.java");
        let dir = std::env::temp_dir().join(format!("rjava-diff-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // A tiny reflective oracle driver, compiled together with Slice.
        let oracle = r#"
import java.io.*;
public final class Oracle {
    public static void main(String[] a) throws Exception {
        var m = Slice.class.getMethod("arith", int.class, int.class, long.class, float.class);
        var r = new BufferedReader(new InputStreamReader(System.in));
        var sb = new StringBuilder();
        String line;
        while ((line = r.readLine()) != null) {
            if (line.isEmpty()) continue;
            String[] p = line.split("\\s+");
            int ia = Integer.parseInt(p[0]), ib = Integer.parseInt(p[1]);
            long c = Long.parseLong(p[2]);
            float d = Float.intBitsToFloat(Integer.parseInt(p[3]));
            sb.append((int) m.invoke(null, ia, ib, c, d)).append('\n');
        }
        System.out.print(sb);
    }
}
"#;
        std::fs::write(dir.join("Oracle.java"), oracle).unwrap();
        let compiled = std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&slice_src)
            .arg(dir.join("Oracle.java"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !compiled {
            // A default JDK is present but cannot target release 21 (e.g. CI's build-test job
            // without setup-java). The Corretto-21 `differential` CI job runs the real gate.
            eprintln!("skipping differential_vs_corretto_21: javac cannot target --release 21");
            return;
        }

        // RustyJVM pipeline.
        let bytes = std::fs::read(dir.join("Slice.class")).unwrap();
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let program = Program::from_class(&cf);

        let inputs = gen_inputs();
        let Some(oracle_results) = run_oracle(&dir, &inputs) else {
            eprintln!("skipping differential_vs_corretto_21: could not run the Corretto oracle");
            return;
        };
        assert_eq!(
            oracle_results.len(),
            inputs.len(),
            "oracle produced one result per input"
        );

        let mut mismatches = 0;
        for (&(a, b, c, d), &expected) in inputs.iter().zip(&oracle_results) {
            let args = [
                Val128::from_i32(a),
                Val128::from_i32(b),
                Val128::from_i64(c),
                Val128::from_f32(d),
            ];
            let got = program
                .call_named("arith", "(IIJF)I", &args)
                .unwrap()
                .unwrap()
                .as_i32();
            if got != expected {
                if mismatches < 10 {
                    eprintln!(
                        "MISMATCH arith({a}, {b}, {c}L, {}f/{:#010x}) = rjvm {got} vs corretto {expected}",
                        d, d.to_bits()
                    );
                }
                mismatches += 1;
            }
        }
        assert_eq!(
            mismatches,
            0,
            "{mismatches}/{} inputs diverged from Corretto 21",
            inputs.len()
        );
        eprintln!("differential OK: {} inputs match Corretto 21", inputs.len());
    }

    fn run_loops_oracle(dir: &std::path::Path, lines: &[String]) -> Option<Vec<i64>> {
        use std::io::Write;
        let stdin_data = lines.join("\n");
        let mut child = std::process::Command::new("java")
            .arg("-cp")
            .arg(dir)
            .arg("LoopsOracle")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .ok()?;
        child.stdin.take()?.write_all(stdin_data.as_bytes()).ok()?;
        let out = child.wait_with_output().ok()?;
        if !out.status.success() {
            eprintln!(
                "loops oracle failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return None;
        }
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().parse::<i64>().ok())
            .collect()
    }

    /// Increment-2 conformance gate: loops (real φ), `iinc`, `if_icmp`, `iand`, `lreturn` — every
    /// result must match Corretto 21. Inputs are bounded per method so the loops terminate.
    #[test]
    fn differential_loops_vs_corretto_21() {
        if !tool_ok("javac") || !tool_ok("java") {
            eprintln!("skipping differential_loops: JDK unavailable");
            return;
        }
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java/Loops.java");
        let dir = std::env::temp_dir().join(format!("rjava-loops-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let oracle = r#"
import java.io.*;
public final class LoopsOracle {
    public static void main(String[] x) throws Exception {
        var br = new BufferedReader(new InputStreamReader(System.in));
        var sb = new StringBuilder();
        String line;
        while ((line = br.readLine()) != null) {
            if (line.isEmpty()) continue;
            String[] p = line.split("\\s+");
            int a = Integer.parseInt(p[1]);
            int b = p.length > 2 ? Integer.parseInt(p[2]) : 0;
            long r;
            switch (p[0]) {
                case "sumTo": r = Loops.sumTo(a); break;
                case "factorial": r = Loops.factorial(a); break;
                case "fib": r = Loops.fib(a); break;
                case "gcd": r = Loops.gcd(a, b); break;
                case "collatz": r = Loops.collatz(a); break;
                default: r = 0;
            }
            sb.append(r).append('\n');
        }
        System.out.print(sb);
    }
}
"#;
        std::fs::write(dir.join("LoopsOracle.java"), oracle).unwrap();
        let compiled = std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&src)
            .arg(dir.join("LoopsOracle.java"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !compiled {
            eprintln!("skipping differential_loops: javac cannot target --release 21");
            return;
        }

        let bytes = std::fs::read(dir.join("Loops.class")).unwrap();
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let program = Program::from_class(&cf);

        // (method, descriptor, a, b, result_is_long)
        let mut cases: Vec<(&str, &str, i32, i32, bool)> = Vec::new();
        for n in [-5, -1, 0, 1, 2, 3, 5, 10, 100, 1000, 10000, 46340] {
            cases.push(("sumTo", "(I)I", n, 0, false));
        }
        for n in [-1, 0, 1, 2, 3, 5, 10, 13, 20, 21, 25, 40] {
            cases.push(("factorial", "(I)J", n, 0, true));
        }
        for n in [0, 1, 2, 3, 5, 10, 20, 45, 46, 47, 90, 92] {
            cases.push(("fib", "(I)I", n, 0, false));
        }
        for (a, b) in [
            (48, 18),
            (18, 48),
            (0, 5),
            (5, 0),
            (1_000_000, 48),
            (-12, 8),
            (12, -8),
            (i32::MAX, 1),
            (i32::MIN, 2),
            (7, 13),
            (270, 192),
            (1, i32::MAX),
        ] {
            cases.push(("gcd", "(II)I", a, b, false));
        }
        for n in [
            1, 2, 3, 6, 7, 9, 27, 55, 97, 171, 703, 871, 6171, 2000, 50000, 100000,
        ] {
            cases.push(("collatz", "(I)I", n, 0, false));
        }
        // Random gcd pairs (Euclid terminates for every int pair).
        let mut s: u64 = 0xABCD_1234_5678_9EF1;
        let mut next = || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            s
        };
        for _ in 0..120 {
            cases.push(("gcd", "(II)I", next() as i32, next() as i32, false));
        }

        let lines: Vec<String> = cases
            .iter()
            .map(|(m, _, a, b, _)| format!("{m} {a} {b}"))
            .collect();
        let Some(oracle_results) = run_loops_oracle(&dir, &lines) else {
            eprintln!("skipping differential_loops: could not run the oracle");
            return;
        };
        assert_eq!(oracle_results.len(), cases.len());

        for ((m, desc, a, b, is_long), &expected) in cases.iter().zip(&oracle_results) {
            let args = if *m == "gcd" {
                vec![Val128::from_i32(*a), Val128::from_i32(*b)]
            } else {
                vec![Val128::from_i32(*a)]
            };
            let r = match program.call_named(m, desc, &args) {
                Ok(Some(v)) => v,
                Ok(None) => panic!("{m}({a}, {b}) returned void"),
                Err(e) => panic!("{m}({a}, {b}) execute failed: {e:?}"),
            };
            let got = if *is_long {
                r.as_i64()
            } else {
                r.as_i32() as i64
            };
            assert_eq!(got, expected, "{m}({a}, {b}) diverged from Corretto 21");
        }
        eprintln!(
            "loops differential OK: {} cases match Corretto 21",
            cases.len()
        );
    }

    fn run_named_oracle(dir: &std::path::Path, class: &str, lines: &[String]) -> Option<Vec<i64>> {
        use std::io::Write;
        let stdin_data = lines.join("\n");
        let mut child = std::process::Command::new("java")
            .arg("-cp")
            .arg(dir)
            .arg(class)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .ok()?;
        child.stdin.take()?.write_all(stdin_data.as_bytes()).ok()?;
        let out = child.wait_with_output().ok()?;
        if !out.status.success() {
            eprintln!(
                "calls oracle failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return None;
        }
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().parse::<i64>().ok())
            .collect()
    }

    /// Increment-2b conformance gate: intra-class `invokestatic`, recursion, and mutual recursion.
    #[test]
    fn differential_calls_vs_corretto_21() {
        if !tool_ok("javac") || !tool_ok("java") {
            eprintln!("skipping differential_calls: JDK unavailable");
            return;
        }
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java/Calls.java");
        let dir = std::env::temp_dir().join(format!("rjava-calls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let oracle = r#"
import java.io.*;
public final class CallsOracle {
    public static void main(String[] x) throws Exception {
        var br = new BufferedReader(new InputStreamReader(System.in));
        var sb = new StringBuilder();
        String line;
        while ((line = br.readLine()) != null) {
            if (line.isEmpty()) continue;
            String[] p = line.split("\\s+");
            int a = Integer.parseInt(p[1]);
            int b = p.length > 2 ? Integer.parseInt(p[2]) : 0;
            int c = p.length > 3 ? Integer.parseInt(p[3]) : 0;
            long r;
            switch (p[0]) {
                case "fib": r = Calls.fib(a); break;
                case "fact": r = Calls.fact(a); break;
                case "isEven": r = Calls.isEven(a); break;
                case "isOdd": r = Calls.isOdd(a); break;
                case "add": r = Calls.add(a, b); break;
                case "addAll": r = Calls.addAll(a, b, c); break;
                default: r = 0;
            }
            sb.append(r).append('\n');
        }
        System.out.print(sb);
    }
}
"#;
        std::fs::write(dir.join("CallsOracle.java"), oracle).unwrap();
        let compiled = std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&src)
            .arg(dir.join("CallsOracle.java"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !compiled {
            eprintln!("skipping differential_calls: javac cannot target --release 21");
            return;
        }

        let bytes = std::fs::read(dir.join("Calls.class")).unwrap();
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let program = Program::from_class(&cf);

        // (method, descriptor, a, b, c, result_is_long)
        let mut cases: Vec<(&str, &str, i32, i32, i32, bool)> = Vec::new();
        for n in 0..=25 {
            cases.push(("fib", "(I)I", n, 0, 0, false));
        }
        for n in 0..=20 {
            cases.push(("fact", "(I)J", n, 0, 0, true));
        }
        for n in [0, 1, 2, 3, 10, 50, 99, 100, 200] {
            // Depth ≈ n, kept inside DEFAULT_MAX_CALL_DEPTH; deeper mutual recursion is covered by
            // `deep_recursion_needs_a_raised_limit_and_a_bigger_stack`.
            cases.push(("isEven", "(I)I", n, 0, 0, false));
            cases.push(("isOdd", "(I)I", n, 0, 0, false));
        }
        for (a, b) in [(3, 4), (-5, 10), (i32::MAX, 1), (i32::MIN, -1), (0, 0)] {
            cases.push(("add", "(II)I", a, b, 0, false));
        }
        for (a, b, c) in [(1, 2, 3), (-1, -2, -3), (i32::MAX, 1, 1), (100, 200, 300)] {
            cases.push(("addAll", "(III)I", a, b, c, false));
        }

        let lines: Vec<String> = cases
            .iter()
            .map(|(m, _, a, b, c, _)| format!("{m} {a} {b} {c}"))
            .collect();
        let Some(oracle_results) = run_named_oracle(&dir, "CallsOracle", &lines) else {
            eprintln!("skipping differential_calls: could not run the oracle");
            return;
        };
        assert_eq!(oracle_results.len(), cases.len());

        for ((m, desc, a, b, c, is_long), &expected) in cases.iter().zip(&oracle_results) {
            let args: Vec<Val128> = match *desc {
                "(II)I" => vec![Val128::from_i32(*a), Val128::from_i32(*b)],
                "(III)I" => vec![
                    Val128::from_i32(*a),
                    Val128::from_i32(*b),
                    Val128::from_i32(*c),
                ],
                _ => vec![Val128::from_i32(*a)],
            };
            let r = program
                .call_named(m, desc, &args)
                .unwrap_or_else(|e| panic!("{m}({a},{b},{c}) failed: {e:?}"))
                .expect("non-void return");
            let got = if *is_long {
                r.as_i64()
            } else {
                r.as_i32() as i64
            };
            assert_eq!(
                got, expected,
                "{m}({a}, {b}, {c}) diverged from Corretto 21"
            );
        }
        eprintln!(
            "calls differential OK: {} cases match Corretto 21",
            cases.len()
        );
    }

    /// Increment-2b StackOverflow seam: unbounded recursion must raise `StackOverflow`, not crash.
    /// Runs on a large stack so the host can hold MAX_CALL_DEPTH frames.
    #[test]
    fn stack_overflow_on_infinite_recursion() {
        if !tool_ok("javac") {
            eprintln!("skipping stack_overflow: javac unavailable");
            return;
        }
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java/Calls.java");
        let dir = std::env::temp_dir().join(format!("rjava-soe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let compiled = std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !compiled {
            eprintln!("skipping stack_overflow: javac cannot target --release 21");
            return;
        }
        let bytes = std::fs::read(dir.join("Calls.class")).unwrap();
        // Regression for a review finding: on a DEFAULT-sized host stack, unbounded guest recursion
        // must return StackOverflow rather than abort the process with a host stack overflow. The
        // thread is deliberately given the default stack size — no 64 MiB crutch.
        let result = std::thread::Builder::new()
            .spawn(move || {
                let cf = rjava_classfile::parse(&bytes).unwrap();
                let program = Program::from_class(&cf); // DEFAULT_MAX_CALL_DEPTH
                program.call_named("deep", "(I)I", &[Val128::from_i32(0)])
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(
            result,
            Err(ExecError::StackOverflow),
            "unbounded recursion must raise StackOverflow, got {result:?}"
        );
        eprintln!("StackOverflow seam OK: deep(0) -> {result:?}");
    }

    /// The documented way to run guest code that recurses deeper than the safe-by-default limit:
    /// give the host thread a bigger stack and raise the limit together. (Until the interpreter
    /// uses an explicit frame stack, guest depth is bounded by host stack space.)
    #[test]
    fn deep_recursion_needs_a_raised_limit_and_a_bigger_stack() {
        if !tool_ok("javac") {
            eprintln!("skipping deep_recursion: javac unavailable");
            return;
        }
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java/Calls.java");
        let dir = std::env::temp_dir().join(format!("rjava-deep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        if !std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            eprintln!("skipping deep_recursion: javac cannot target --release 21");
            return;
        }
        let bytes = std::fs::read(dir.join("Calls.class")).unwrap();
        let got = std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(move || {
                let cf = rjava_classfile::parse(&bytes).unwrap();
                let program = Program::from_class(&cf).with_max_call_depth(4000);
                // isEven(3000) recurses ~3000 frames — far past DEFAULT_MAX_CALL_DEPTH.
                program
                    .call_named("isEven", "(I)I", &[Val128::from_i32(3000)])
                    .unwrap()
                    .unwrap()
                    .as_i32()
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(got, 1, "isEven(3000) is true");
        eprintln!("raised-limit deep recursion OK: isEven(3000) -> {got}");
    }

    /// Stack-φ conformance: native ternary `?:` (including nested) is lowered via φ over
    /// operand-stack slots; every result must match Corretto 21.
    #[test]
    fn differential_ternary_vs_corretto_21() {
        if !tool_ok("javac") || !tool_ok("java") {
            eprintln!("skipping differential_ternary: JDK unavailable");
            return;
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/java/Ternary.java");
        let dir = std::env::temp_dir().join(format!("rjava-tern-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let oracle = r#"
import java.io.*;
public final class TernaryOracle {
    public static void main(String[] x) throws Exception {
        var br = new BufferedReader(new InputStreamReader(System.in));
        var sb = new StringBuilder();
        String line;
        while ((line = br.readLine()) != null) {
            if (line.isEmpty()) continue;
            String[] p = line.split("\\s+");
            int a = Integer.parseInt(p[1]);
            int b = p.length > 2 ? Integer.parseInt(p[2]) : 0;
            int c = p.length > 3 ? Integer.parseInt(p[3]) : 0;
            int r;
            switch (p[0]) {
                case "max": r = Ternary.max(a, b); break;
                case "min": r = Ternary.min(a, b); break;
                case "abs": r = Ternary.abs(a); break;
                case "sign": r = Ternary.sign(a); break;
                case "clamp": r = Ternary.clamp(a, b, c); break;
                case "med3": r = Ternary.med3(a, b, c); break;
                case "select": r = Ternary.select(a, b, c); break;
                default: r = 0;
            }
            sb.append(r).append('\n');
        }
        System.out.print(sb);
    }
}
"#;
        std::fs::write(dir.join("TernaryOracle.java"), oracle).unwrap();
        let compiled = std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&src)
            .arg(dir.join("TernaryOracle.java"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !compiled {
            eprintln!("skipping differential_ternary: javac cannot target --release 21");
            return;
        }
        let bytes = std::fs::read(dir.join("Ternary.class")).unwrap();
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let program = Program::from_class(&cf);

        let vals = [
            0,
            1,
            -1,
            2,
            -2,
            5,
            -5,
            100,
            -100,
            42,
            -42,
            i32::MIN,
            i32::MAX,
            i32::MIN + 1,
            i32::MAX - 1,
        ];
        let mut cases: Vec<(&str, &str, i32, i32, i32)> = Vec::new();
        for &n in &vals {
            cases.push(("abs", "(I)I", n, 0, 0));
            cases.push(("sign", "(I)I", n, 0, 0));
        }
        for &a in &vals {
            for &b in &[0, 1, -1, 100, -100, i32::MIN, i32::MAX] {
                cases.push(("max", "(II)I", a, b, 0));
                cases.push(("min", "(II)I", a, b, 0));
            }
        }
        let mut s: u64 = 0x1357_9BDF_2468_ACE0;
        let mut next = || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            s
        };
        for _ in 0..80 {
            let (a, b, c) = (next() as i32, next() as i32, next() as i32);
            cases.push(("clamp", "(III)I", a, b, c));
            cases.push(("med3", "(III)I", a, b, c));
            cases.push(("select", "(III)I", a, b, c));
        }

        let lines: Vec<String> = cases
            .iter()
            .map(|(m, _, a, b, c)| format!("{m} {a} {b} {c}"))
            .collect();
        let Some(oracle_results) = run_named_oracle(&dir, "TernaryOracle", &lines) else {
            eprintln!("skipping differential_ternary: could not run the oracle");
            return;
        };
        assert_eq!(oracle_results.len(), cases.len());

        for ((m, desc, a, b, c), &expected) in cases.iter().zip(&oracle_results) {
            let args: Vec<Val128> = match *desc {
                "(I)I" => vec![Val128::from_i32(*a)],
                "(II)I" => vec![Val128::from_i32(*a), Val128::from_i32(*b)],
                _ => vec![
                    Val128::from_i32(*a),
                    Val128::from_i32(*b),
                    Val128::from_i32(*c),
                ],
            };
            let r = program
                .call_named(m, desc, &args)
                .unwrap_or_else(|e| panic!("{m}({a},{b},{c}) failed: {e:?}"))
                .expect("non-void return")
                .as_i32() as i64;
            assert_eq!(r, expected, "{m}({a}, {b}, {c}) diverged from Corretto 21");
        }
        eprintln!(
            "ternary differential OK: {} cases match Corretto 21",
            cases.len()
        );
    }

    /// Interaction (联动) gate: loops + recursive/iterative calls + ternary + int/long arithmetic
    /// combined in single methods, all matching Corretto 21.
    #[test]
    fn differential_mixed_vs_corretto_21() {
        if !tool_ok("javac") || !tool_ok("java") {
            eprintln!("skipping differential_mixed: JDK unavailable");
            return;
        }
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java/Mixed.java");
        let dir = std::env::temp_dir().join(format!("rjava-mixed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let oracle = r#"
import java.io.*;
public final class MixedOracle {
    public static void main(String[] x) throws Exception {
        var br = new BufferedReader(new InputStreamReader(System.in));
        var sb = new StringBuilder();
        String line;
        while ((line = br.readLine()) != null) {
            if (line.isEmpty()) continue;
            String[] p = line.split("\\s+");
            int a = Integer.parseInt(p[1]);
            int b = p.length > 2 ? Integer.parseInt(p[2]) : 0;
            long r;
            switch (p[0]) {
                case "fib": r = Mixed.fib(a); break;
                case "gcd": r = Mixed.gcd(a, b); break;
                case "sumFibSigned": r = Mixed.sumFibSigned(a); break;
                case "totient": r = Mixed.totient(a); break;
                case "collatzLen": r = Mixed.collatzLen(a); break;
                default: r = 0;
            }
            sb.append(r).append('\n');
        }
        System.out.print(sb);
    }
}
"#;
        std::fs::write(dir.join("MixedOracle.java"), oracle).unwrap();
        let compiled = std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&src)
            .arg(dir.join("MixedOracle.java"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !compiled {
            eprintln!("skipping differential_mixed: javac cannot target --release 21");
            return;
        }
        let bytes = std::fs::read(dir.join("Mixed.class")).unwrap();
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let program = Program::from_class(&cf);

        // (method, descriptor, a, b, result_is_long)
        let mut cases: Vec<(&str, &str, i32, i32, bool)> = Vec::new();
        for n in 0..=25 {
            cases.push(("fib", "(I)I", n, 0, false));
        }
        for (a, b) in [
            (48, 18),
            (0, 7),
            (7, 0),
            (270, 192),
            (i32::MAX, 3),
            (-12, 8),
        ] {
            cases.push(("gcd", "(II)I", a, b, false));
        }
        for n in 0..=22 {
            cases.push(("sumFibSigned", "(I)J", n, 0, true));
        }
        for n in [1, 2, 3, 6, 10, 12, 36, 97, 100, 200, 360] {
            cases.push(("totient", "(I)I", n, 0, false));
        }
        for n in [1, 2, 3, 6, 7, 27, 97, 255, 511, 1000] {
            cases.push(("collatzLen", "(I)I", n, 0, false));
        }

        let lines: Vec<String> = cases
            .iter()
            .map(|(m, _, a, b, _)| format!("{m} {a} {b}"))
            .collect();
        let Some(oracle_results) = run_named_oracle(&dir, "MixedOracle", &lines) else {
            eprintln!("skipping differential_mixed: could not run the oracle");
            return;
        };
        assert_eq!(oracle_results.len(), cases.len());

        for ((m, desc, a, b, is_long), &expected) in cases.iter().zip(&oracle_results) {
            let args: Vec<Val128> = if *desc == "(II)I" {
                vec![Val128::from_i32(*a), Val128::from_i32(*b)]
            } else {
                vec![Val128::from_i32(*a)]
            };
            let r = program
                .call_named(m, desc, &args)
                .unwrap_or_else(|e| panic!("{m}({a},{b}) failed: {e:?}"))
                .expect("non-void return");
            let got = if *is_long {
                r.as_i64()
            } else {
                r.as_i32() as i64
            };
            assert_eq!(got, expected, "{m}({a}, {b}) diverged from Corretto 21");
        }
        eprintln!(
            "mixed (interaction) differential OK: {} cases match Corretto 21",
            cases.len()
        );
    }

    /// Increment-3 conformance gate: `new`/getfield/putfield/invokespecial on local (S1) objects,
    /// including objects mutated in a loop; every result must match Corretto 21.
    #[test]
    fn differential_objects_vs_corretto_21() {
        if !tool_ok("javac") || !tool_ok("java") {
            eprintln!("skipping differential_objects: JDK unavailable");
            return;
        }
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java/Point.java");
        let dir = std::env::temp_dir().join(format!("rjava-obj-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let oracle = r#"
import java.io.*;
public final class PointOracle {
    public static void main(String[] x) throws Exception {
        var br = new BufferedReader(new InputStreamReader(System.in));
        var sb = new StringBuilder();
        String line;
        while ((line = br.readLine()) != null) {
            if (line.isEmpty()) continue;
            String[] p = line.split("\\s+");
            int a = p.length > 1 ? Integer.parseInt(p[1]) : 0;
            int b = p.length > 2 ? Integer.parseInt(p[2]) : 0;
            int c = p.length > 3 ? Integer.parseInt(p[3]) : 0;
            int d = p.length > 4 ? Integer.parseInt(p[4]) : 0;
            int r;
            switch (p[0]) {
                case "normSq": r = Point.normSq(a, b); break;
                case "manhattan": r = Point.manhattan(a, b, c, d); break;
                case "defaults": r = Point.defaults(); break;
                case "accumulate": r = Point.accumulate(a); break;
                default: r = 0;
            }
            sb.append(r).append('\n');
        }
        System.out.print(sb);
    }
}
"#;
        std::fs::write(dir.join("PointOracle.java"), oracle).unwrap();
        let compiled = std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&src)
            .arg(dir.join("PointOracle.java"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !compiled {
            eprintln!("skipping differential_objects: javac cannot target --release 21");
            return;
        }
        let bytes = std::fs::read(dir.join("Point.class")).unwrap();
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let program = Program::from_class(&cf);

        // (method, descriptor, a, b, c, d)
        let mut cases: Vec<(&str, &str, i32, i32, i32, i32)> = Vec::new();
        cases.push(("defaults", "()I", 0, 0, 0, 0));
        for (a, b) in [
            (3, 4),
            (0, 0),
            (-5, 12),
            (i32::MAX, 1),
            (46340, 46340),
            (-7, -8),
        ] {
            cases.push(("normSq", "(II)I", a, b, 0, 0));
        }
        for (a, b, c, d) in [
            (0, 0, 3, 4),
            (1, 1, -1, -1),
            (10, 20, 30, 40),
            (i32::MIN, 0, 0, 0),
            (-5, 5, 5, -5),
        ] {
            cases.push(("manhattan", "(IIII)I", a, b, c, d));
        }
        for n in [0, 1, 2, 5, 10, 100, 1000] {
            cases.push(("accumulate", "(I)I", n, 0, 0, 0));
        }

        let lines: Vec<String> = cases
            .iter()
            .map(|(m, _, a, b, c, d)| format!("{m} {a} {b} {c} {d}"))
            .collect();
        let Some(oracle_results) = run_named_oracle(&dir, "PointOracle", &lines) else {
            eprintln!("skipping differential_objects: could not run the oracle");
            return;
        };
        assert_eq!(oracle_results.len(), cases.len());

        for ((m, desc, a, b, c, d), &expected) in cases.iter().zip(&oracle_results) {
            let args: Vec<Val128> = match *desc {
                "()I" => vec![],
                "(I)I" => vec![Val128::from_i32(*a)],
                "(II)I" => vec![Val128::from_i32(*a), Val128::from_i32(*b)],
                _ => vec![
                    Val128::from_i32(*a),
                    Val128::from_i32(*b),
                    Val128::from_i32(*c),
                    Val128::from_i32(*d),
                ],
            };
            let r = program
                .call_named(m, desc, &args)
                .unwrap_or_else(|e| panic!("{m}({a},{b},{c},{d}) failed: {e:?}"))
                .expect("non-void return")
                .as_i32() as i64;
            assert_eq!(
                r, expected,
                "{m}({a}, {b}, {c}, {d}) diverged from Corretto 21"
            );
        }
        eprintln!(
            "objects differential OK: {} cases match Corretto 21",
            cases.len()
        );
    }

    /// Increment-5 conformance gate: escape analysis (S1 vs S2) and the reference-count fast path
    /// must not change observable results — every case still matches Corretto 21.
    #[test]
    fn differential_escape_vs_corretto_21() {
        if !tool_ok("javac") || !tool_ok("java") {
            eprintln!("skipping differential_escape: JDK unavailable");
            return;
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/java/Escape.java");
        let dir = std::env::temp_dir().join(format!("rjava-esc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let oracle = r#"
import java.io.*;
public final class EscapeOracle {
    public static void main(String[] x) throws Exception {
        var br = new BufferedReader(new InputStreamReader(System.in));
        var sb = new StringBuilder();
        String line;
        while ((line = br.readLine()) != null) {
            if (line.isEmpty()) continue;
            String[] p = line.split("\\s+");
            int a = Integer.parseInt(p[1]);
            int b = p.length > 2 ? Integer.parseInt(p[2]) : 0;
            int r;
            switch (p[0]) {
                case "local": r = Escape.local(a); break;
                case "useReturned": r = Escape.useReturned(a); break;
                case "stored": r = Escape.stored(a, b); break;
                case "chain": r = Escape.chain(a); break;
                case "churn": r = Escape.churn(a); break;
                default: r = 0;
            }
            sb.append(r).append('\n');
        }
        System.out.print(sb);
    }
}
"#;
        std::fs::write(dir.join("EscapeOracle.java"), oracle).unwrap();
        if !std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&src)
            .arg(dir.join("EscapeOracle.java"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            eprintln!("skipping differential_escape: javac cannot target --release 21");
            return;
        }
        let bytes = std::fs::read(dir.join("Escape.class")).unwrap();
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let program = Program::from_class(&cf);

        let mut cases: Vec<(&str, &str, i32, i32)> = Vec::new();
        for n in [0, 1, 2, 7, -3, 100, i32::MAX, i32::MIN] {
            cases.push(("local", "(I)I", n, 0));
            cases.push(("useReturned", "(I)I", n, 0));
        }
        for (a, b) in [(1, 2), (0, 0), (-5, 9), (i32::MAX, 1), (100, 200)] {
            cases.push(("stored", "(II)I", a, b));
        }
        for n in [0, 1, 2, 5, 10, 50, 200] {
            cases.push(("chain", "(I)I", n, 0));
            cases.push(("churn", "(I)I", n, 0));
        }

        let lines: Vec<String> = cases
            .iter()
            .map(|(m, _, a, b)| format!("{m} {a} {b}"))
            .collect();
        let Some(expected) = run_named_oracle(&dir, "EscapeOracle", &lines) else {
            eprintln!("skipping differential_escape: could not run the oracle");
            return;
        };
        assert_eq!(expected.len(), cases.len());

        for ((m, desc, a, b), &want) in cases.iter().zip(&expected) {
            let args: Vec<Val128> = if *desc == "(II)I" {
                vec![Val128::from_i32(*a), Val128::from_i32(*b)]
            } else {
                vec![Val128::from_i32(*a)]
            };
            let got = program
                .call_named(m, desc, &args)
                .unwrap_or_else(|e| panic!("{m}({a},{b}) failed: {e:?}"))
                .unwrap_or_else(|| panic!("{m}({a},{b}) returned void"))
                .as_i32() as i64;
            assert_eq!(got, want, "{m}({a}, {b}) diverged from Corretto 21");
        }
        eprintln!(
            "escape/rc differential OK: {} cases match Corretto 21",
            cases.len()
        );
    }

    /// The reference-count fast path must actually reclaim: after a call tree that allocates
    /// hundreds of objects — local (S1, RAII) and escaping (S2, counted) alike — the heap must be
    /// empty again, and the count must have driven that (§5.1, §5.2, §6.2).
    #[test]
    fn objects_are_reclaimed_when_unreferenced() {
        if !tool_ok("javac") {
            eprintln!("skipping reclamation test: javac unavailable");
            return;
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/java/Escape.java");
        let dir = std::env::temp_dir().join(format!("rjava-rc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        if !std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            eprintln!("skipping reclamation test: javac cannot target --release 21");
            return;
        }
        let bytes = std::fs::read(dir.join("Escape.class")).unwrap();
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let program = Program::from_class(&cf);

        for (m, desc, arg) in [
            ("churn", "(I)I", 300),     // S1 objects, reclaimed by RAII at scope exit
            ("chain", "(I)I", 200),     // an S2 chain, reclaimed by cascading rc release
            ("useReturned", "(I)I", 5), // an S2 object handed across a frame boundary
            ("local", "(I)I", 3),
        ] {
            let mut heap = Heap::new();
            let idx = program.method_index(m, desc).expect("method present");
            program
                .call(program.entry, idx, &[Val128::from_i32(arg)], 0, &mut heap)
                .unwrap_or_else(|e| panic!("{m}({arg}) failed: {e:?}"));
            assert_eq!(
                heap.live(),
                0,
                "{m}({arg}) leaked {} objects — every allocation must be reclaimed once \
                 unreferenced",
                heap.live()
            );
        }
        eprintln!("reference-count reclamation OK: no leaks across churn/chain/useReturned/local");
    }

    /// Increment-6 conformance gate: a real class hierarchy — cross-class `new`, superclass
    /// constructors, **virtual dispatch on the runtime class**, cross-class static calls and field
    /// reads, `instanceof`/`checkcast`, and static fields initialised by `<clinit>`.
    #[test]
    fn differential_hierarchy_vs_corretto_21() {
        if !tool_ok("javac") || !tool_ok("java") {
            eprintln!("skipping differential_hierarchy: JDK unavailable");
            return;
        }
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java");
        let dir = std::env::temp_dir().join(format!("rjava-hier-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let oracle = r#"
import java.io.*;
public final class ShapesOracle {
    public static void main(String[] x) throws Exception {
        var br = new BufferedReader(new InputStreamReader(System.in));
        var sb = new StringBuilder();
        String line;
        while ((line = br.readLine()) != null) {
            if (line.isEmpty()) continue;
            String[] p = line.split("\\s+");
            int a = Integer.parseInt(p[1]);
            int b = p.length > 2 ? Integer.parseInt(p[2]) : 0;
            int r;
            switch (p[0]) {
                case "squareArea": r = Shapes.squareArea(a); break;
                case "rectArea": r = Shapes.rectArea(a, b); break;
                case "polymorphic": r = Shapes.polymorphic(a, b); break;
                case "described": r = Shapes.described(a, b); break;
                case "superCtor": r = Shapes.superCtor(a); break;
                case "instanceOf": r = Shapes.instanceOf(a); break;
                case "statics": r = Shapes.statics(a); break;
                default: r = 0;
            }
            sb.append(r).append('\n');
        }
        System.out.print(sb);
    }
}
"#;
        std::fs::write(dir.join("ShapesOracle.java"), oracle).unwrap();
        let ok = std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(src_dir.join("Shape.java"))
            .arg(src_dir.join("Square.java"))
            .arg(src_dir.join("Rect.java"))
            .arg(src_dir.join("Shapes.java"))
            .arg(dir.join("ShapesOracle.java"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("skipping differential_hierarchy: javac cannot target --release 21");
            return;
        }

        let mut cases: Vec<(&str, &str, i32, i32)> = Vec::new();
        for n in [0, 1, 2, 3, 7, -4, 100, 46341] {
            cases.push(("squareArea", "(I)I", n, 0));
            cases.push(("superCtor", "(I)I", n, 0));
            cases.push(("instanceOf", "(I)I", n, 0));
        }
        for (a, b) in [(2, 3), (0, 0), (-5, 4), (10, 10), (i32::MAX, 2)] {
            cases.push(("rectArea", "(II)I", a, b));
            cases.push(("polymorphic", "(II)I", a, b));
            cases.push(("described", "(II)I", a, b));
        }
        // `statics` mutates a static field, so each call depends on the previous one — the oracle
        // and RustyJVM must see the same running total, in the same order.
        for n in [1, 2, 3, 10, -5] {
            cases.push(("statics", "(I)I", n, 0));
        }

        let lines: Vec<String> = cases
            .iter()
            .map(|(m, _, a, b)| format!("{m} {a} {b}"))
            .collect();
        let Some(expected) = run_named_oracle(&dir, "ShapesOracle", &lines) else {
            eprintln!("skipping differential_hierarchy: could not run the oracle");
            return;
        };
        assert_eq!(expected.len(), cases.len());

        // One program for the whole run so static state accumulates exactly as it does in the JVM.
        let program = Program::from_classpath(&dir, "Shapes").expect("classpath links");
        for ((m, desc, a, b), &want) in cases.iter().zip(&expected) {
            let args: Vec<Val128> = if *desc == "(II)I" {
                vec![Val128::from_i32(*a), Val128::from_i32(*b)]
            } else {
                vec![Val128::from_i32(*a)]
            };
            let got = program
                .call_named(m, desc, &args)
                .unwrap_or_else(|e| panic!("{m}({a},{b}) failed: {e:?}"))
                .unwrap_or_else(|| panic!("{m}({a},{b}) returned void"))
                .as_i32() as i64;
            assert_eq!(got, want, "{m}({a}, {b}) diverged from Corretto 21");
        }
        eprintln!(
            "hierarchy differential OK: {} cases match Corretto 21",
            cases.len()
        );
    }

    /// Regression for a review finding: a frame that leaves **abruptly** must release what it
    /// owned, just as a returning frame does. Otherwise a throw leaks its S1 handles and S2
    /// references, and a reused bounded heap is exhausted by repeating the same call.
    #[test]
    fn an_abrupt_exit_releases_the_frames_references() {
        if !tool_ok("javac") {
            eprintln!("skipping abrupt-exit test: javac unavailable");
            return;
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/java/Abrupt.java");
        let dir = std::env::temp_dir().join(format!("rjava-abrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        if !std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            eprintln!("skipping abrupt-exit test: javac cannot target --release 21");
            return;
        }
        let bytes = std::fs::read(dir.join("Abrupt.class")).unwrap();
        let cf = rjava_classfile::parse(&bytes).unwrap();
        let program = Program::from_class(&cf);
        let idx = program
            .method_index("throwsAfterAllocating", "(I)I")
            .expect("method present");

        // A heap just big enough for one call's two objects: if the failed call kept them, the
        // second call would fail with OutOfMemory instead of the same NullPointer.
        let mut heap = Heap::with_limit(2);
        for attempt in 1..=5 {
            let r = program.call(
                program.entry,
                idx,
                &[Val128::from_i32(attempt)],
                0,
                &mut heap,
            );
            assert_eq!(
                r,
                Err(ExecError::NullPointer),
                "attempt {attempt} should fail on the null receiver, not run out of heap"
            );
            assert_eq!(
                heap.live(),
                0,
                "attempt {attempt} leaked {} objects out of an abruptly-exited frame",
                heap.live()
            );
        }
        eprintln!("abrupt-exit teardown OK: 5 failed calls on a 2-object heap, nothing retained");
    }

    /// Increment-7 conformance gate: `rustystd`'s builtin classes. Every effect here is **standard
    /// output**, so the test compares RustyJVM's bytes against Corretto's byte for byte — literal
    /// interning identity, `String` methods (including the JLS `hashCode` polynomial), and
    /// `System.out` formatting all have to agree.
    #[test]
    fn differential_stdout_vs_corretto_21() {
        if !tool_ok("javac") || !tool_ok("java") {
            eprintln!("skipping differential_stdout: JDK unavailable");
            return;
        }
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java/Hello.java");
        let dir = std::env::temp_dir().join(format!("rjava-hello-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // The oracle calls the same methods in the same order, on one JVM, so interning and any
        // other cross-call state is exercised identically.
        let oracle = r#"
public final class HelloOracle {
    public static void main(String[] a) {
        Hello.greet();
        Hello.strings();
        Hello.interning();
        Hello.primitives();
        Hello.unicode();
    }
}
"#;
        std::fs::write(dir.join("HelloOracle.java"), oracle).unwrap();
        if !std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&dir)
            .arg(&src)
            .arg(dir.join("HelloOracle.java"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            eprintln!("skipping differential_stdout: javac cannot target --release 21");
            return;
        }
        let out = std::process::Command::new("java")
            .arg("-cp")
            .arg(&dir)
            .arg("HelloOracle")
            .output()
            .expect("oracle runs");
        assert!(
            out.status.success(),
            "oracle failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // Corretto writes '\n'; normalise so the comparison is about content, not line endings.
        let expected = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");

        let program = Program::from_classpath(&dir, "Hello")
            .expect("classpath links")
            .capturing();
        for m in ["greet", "strings", "interning", "primitives", "unicode"] {
            program
                .call_named(m, "()V", &[])
                .unwrap_or_else(|e| panic!("Hello.{m}() failed: {e:?}"));
        }
        let got = String::from_utf8(program.take_output()).expect("guest output is UTF-8");

        assert_eq!(
            got, expected,
            "stdout diverged from Corretto 21\n--- rustyjvm ---\n{got}\n--- corretto ---\n{expected}"
        );
        eprintln!(
            "stdout differential OK: {} bytes match Corretto 21",
            got.len()
        );
    }
}
