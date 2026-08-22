//! rjava-loader — the class registry, field/method layout, and the initialisation state machine
//! (RJVM-SPEC-001 §8).
//!
//! A class's runtime identity is `(name, defining loader)` (§8.1); increment 6 has a single
//! bootstrap namespace, so a [`ClassId`] is an index into this registry and names are unique within
//! it. Parent delegation and multiple loaders slot in here without changing consumers.
//!
//! **Resolution is symbolic and lazy** (§8.3): the IR keeps constant-pool indices, and the
//! interpreter resolves them through this registry at first use, so nothing about class identity or
//! dispatch is baked at build time (P-3, §13.4). Virtual dispatch therefore goes through a
//! **runtime-mutable table** — adding a class adds table entries and never invalidates compiled
//! code, which is why RustyJVM needs no deoptimisation engine (§13.4).
//!
//! Class *loading* here is eager over a classpath directory: the registry parses and links whatever
//! it finds up front. That is an implementation choice, not an observable one — what JVMS makes
//! observable is *initialisation* timing, and `<clinit>` still runs lazily at first active use
//! (§8.5, [`InitState`]).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;

use rjava_classfile::ClassFile;
use rjava_core::{ClassId, Val128};
use rjava_ir::BuiltMethod;

/// Failures while loading or linking a class.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoaderError {
    #[error("class file {0} could not be read")]
    Io(String),
    #[error("class file {0} is malformed")]
    Malformed(String),
    #[error("class {0} was not found on the classpath")]
    NotFound(String),
    #[error("class {0} has a circular superclass chain")]
    CircularHierarchy(String),
}

/// Class initialisation state (§8.5, JVMS §5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitState {
    /// Verified and linked, `<clinit>` not run.
    Loaded,
    /// A virtual thread is running `<clinit>`; the same vt re-entering sees the partially
    /// initialised state instead of deadlocking (JVMS §5.5 pass-through).
    Initializing(u32),
    Initialized,
    /// `<clinit>` threw; subsequent use raises `NoClassDefFoundError`.
    Failed,
}

/// One virtual-dispatch table entry: the class that defines the most-derived override, and the
/// index of the method within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VEntry {
    pub class: ClassId,
    pub method: u16,
}

/// A loaded, linked class.
pub struct LoadedClass {
    pub name: String,
    pub cf: ClassFile,
    pub super_id: Option<ClassId>,
    /// Verified + built methods, indexed as in the class file (`None` if it could not be built).
    pub methods: Vec<Option<BuiltMethod>>,
    /// Where this class's own instance fields begin; everything below is inherited (§8).
    pub field_base: u16,
    /// Type-appropriate zero values for a whole instance, inherited fields first.
    pub field_defaults: Vec<Val128>,
    /// `(name, descriptor)` of an instance field → its absolute slot in an instance.
    field_index: HashMap<(String, String), u16>,
    /// Static field storage, mutated by `<clinit>` and `putstatic` while the registry is shared.
    pub statics: RefCell<Vec<Val128>>,
    static_index: HashMap<(String, String), usize>,
    /// Virtual dispatch table: inherited entries first, overrides replaced in place (§13.4).
    pub vtable: Vec<VEntry>,
    vindex: HashMap<(String, String), usize>,
    init: Cell<InitState>,
}

impl LoadedClass {
    /// The absolute instance slot of an instance field declared by this class or inherited.
    pub fn field_slot(&self, name: &str, desc: &str) -> Option<u16> {
        self.field_index
            .get(&(name.to_string(), desc.to_string()))
            .copied()
    }

    /// The index of a static field in [`LoadedClass::statics`].
    pub fn static_slot(&self, name: &str, desc: &str) -> Option<usize> {
        self.static_index
            .get(&(name.to_string(), desc.to_string()))
            .copied()
    }

    /// The virtual-dispatch slot for a method signature, if it is virtual.
    pub fn vslot(&self, name: &str, desc: &str) -> Option<usize> {
        self.vindex
            .get(&(name.to_string(), desc.to_string()))
            .copied()
    }

    /// The index of a method declared directly by this class.
    pub fn method_index(&self, name: &str, desc: &str) -> Option<u16> {
        self.cf
            .methods
            .iter()
            .position(|m| {
                m.name(&self.cf.constant_pool) == Some(name)
                    && m.descriptor(&self.cf.constant_pool) == Some(desc)
            })
            .map(|i| i as u16)
    }

    pub fn init_state(&self) -> InitState {
        self.init.get()
    }
    pub fn set_init_state(&self, s: InitState) {
        self.init.set(s);
    }
}

/// The class registry: the mapping from a class name to its loaded form (§8.4).
pub struct ClassRegistry {
    /// Loading is eager — every class is linked before execution — so this never grows while the
    /// interpreter holds a shared borrow of the registry.
    classes: Vec<LoadedClass>,
    by_name: HashMap<String, ClassId>,
}

const ACC_STATIC: u16 = 0x0008;

fn is_static(flags: u16) -> bool {
    flags & ACC_STATIC != 0
}

/// The JVMS default value for a field of the given descriptor.
fn field_default(desc: &str) -> Val128 {
    match desc.as_bytes().first() {
        Some(b'J') => Val128::from_i64(0),
        Some(b'F') => Val128::from_f32(0.0),
        Some(b'D') => Val128::from_f64(0.0),
        Some(b'L') | Some(b'[') => Val128::null(),
        _ => Val128::from_i32(0), // B, C, I, S, Z
    }
}

impl ClassRegistry {
    pub fn new() -> Self {
        ClassRegistry {
            classes: Vec::new(),
            by_name: HashMap::new(),
        }
    }

    /// Load and link every `.class` file in `dir`.
    ///
    /// Classes are parsed first, then linked in superclass-before-subclass order so that field
    /// layout and the dispatch table can extend the superclass's.
    pub fn load_dir(&mut self, dir: &Path) -> Result<(), LoaderError> {
        let entries = std::fs::read_dir(dir)
            .map_err(|_| LoaderError::Io(dir.display().to_string()))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("class"));

        let mut parsed: Vec<ClassFile> = Vec::new();
        for path in entries {
            let bytes =
                std::fs::read(&path).map_err(|_| LoaderError::Io(path.display().to_string()))?;
            let cf = rjava_classfile::parse(&bytes)
                .map_err(|_| LoaderError::Malformed(path.display().to_string()))?;
            parsed.push(cf);
        }

        // Link supers first: a subclass's layout and dispatch table extend its superclass's.
        let mut pending: Vec<ClassFile> = parsed;
        let mut progress = true;
        while progress && !pending.is_empty() {
            progress = false;
            let mut still_pending = Vec::new();
            for cf in pending {
                let super_ready = match cf.super_class_name() {
                    // `java/lang/Object` has no class file here; it is the implicit root.
                    None | Some("java/lang/Object") => true,
                    Some(s) => self.by_name.contains_key(s),
                };
                if super_ready {
                    self.link(cf);
                    progress = true;
                } else {
                    still_pending.push(cf);
                }
            }
            pending = still_pending;
        }
        if let Some(cf) = pending.first() {
            // A superclass that never became ready is either missing or part of a cycle.
            return Err(LoaderError::CircularHierarchy(
                cf.this_class_name().unwrap_or("?").to_string(),
            ));
        }
        Ok(())
    }

    /// Link one parsed class: verify + build its methods, lay out its fields, and extend its
    /// superclass's dispatch table with its own virtual methods (§13.4).
    fn link(&mut self, cf: ClassFile) {
        let name = cf.this_class_name().unwrap_or("?").to_string();
        let super_id = cf
            .super_class_name()
            .and_then(|s| self.by_name.get(s))
            .copied();

        let (mut field_defaults, mut field_index, mut vtable, mut vindex) = match super_id {
            Some(sid) => {
                let sup = &self.classes[sid.0 as usize];
                (
                    sup.field_defaults.clone(),
                    sup.field_index.clone(),
                    sup.vtable.clone(),
                    sup.vindex.clone(),
                )
            }
            None => (Vec::new(), HashMap::new(), Vec::new(), HashMap::new()),
        };
        let field_base = field_defaults.len() as u16;

        let id = ClassId(self.classes.len() as u32);

        // Instance fields extend the superclass's layout; statics get their own storage.
        let mut statics = Vec::new();
        let mut static_index = HashMap::new();
        for f in &cf.fields {
            let (Some(fname), Some(fdesc)) =
                (f.name(&cf.constant_pool), f.descriptor(&cf.constant_pool))
            else {
                continue;
            };
            let key = (fname.to_string(), fdesc.to_string());
            if is_static(f.access_flags) {
                static_index.insert(key, statics.len());
                statics.push(field_default(fdesc));
            } else {
                field_index.insert(key, field_defaults.len() as u16);
                field_defaults.push(field_default(fdesc));
            }
        }

        // Virtual methods: an identical (name, descriptor) overrides the inherited slot in place,
        // so every existing call site keeps working — the table is mutable, dispatch is not baked.
        for (i, m) in cf.methods.iter().enumerate() {
            let (Some(mname), Some(mdesc)) =
                (m.name(&cf.constant_pool), m.descriptor(&cf.constant_pool))
            else {
                continue;
            };
            if is_static(m.access_flags) || mname == "<init>" || mname == "<clinit>" {
                continue; // not virtually dispatched
            }
            let entry = VEntry {
                class: id,
                method: i as u16,
            };
            let key = (mname.to_string(), mdesc.to_string());
            match vindex.get(&key) {
                Some(&slot) => vtable[slot] = entry, // override
                None => {
                    vindex.insert(key, vtable.len());
                    vtable.push(entry);
                }
            }
        }

        let methods = cf
            .methods
            .iter()
            .map(|m| {
                rjava_verify::verify_method(&cf, m)
                    .ok()
                    .and_then(|vm| rjava_ir::build(&vm, &cf).ok())
            })
            .collect();

        self.by_name.insert(name.clone(), id);
        self.classes.push(LoadedClass {
            name,
            cf,
            super_id,
            methods,
            field_base,
            field_defaults,
            field_index,
            statics: RefCell::new(statics),
            static_index,
            vtable,
            vindex,
            init: Cell::new(InitState::Loaded),
        });
    }

    pub fn get(&self, id: ClassId) -> Option<&LoadedClass> {
        self.classes.get(id.0 as usize)
    }

    pub fn by_name(&self, name: &str) -> Option<ClassId> {
        self.by_name.get(name).copied()
    }

    pub fn len(&self) -> usize {
        self.classes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.classes.is_empty()
    }

    /// Whether `sub` is `sup` or descends from it — the `instanceof`/`checkcast` relation for the
    /// class hierarchy (interfaces arrive with the full loader).
    pub fn is_subclass_of(&self, sub: ClassId, sup: ClassId) -> bool {
        let mut cur = Some(sub);
        while let Some(c) = cur {
            if c == sup {
                return true;
            }
            cur = self.get(c).and_then(|k| k.super_id);
        }
        false
    }
}

impl Default for ClassRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(names: &[&str], tag: &str) -> Option<std::path::PathBuf> {
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java");
        let out = std::env::temp_dir().join(format!("rjava-loader-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&out).ok()?;
        let mut cmd = std::process::Command::new("javac");
        cmd.args(["--release", "21", "-d"]).arg(&out);
        for n in names {
            cmd.arg(src_dir.join(format!("{n}.java")));
        }
        cmd.status().ok()?.success().then_some(out)
    }

    #[test]
    fn links_a_hierarchy_with_layout_and_dispatch() {
        let Some(dir) = compile(&["Shape", "Square", "Rect", "Shapes"], "hier") else {
            eprintln!("skipping loader test: javac unavailable");
            return;
        };
        let mut reg = ClassRegistry::new();
        reg.load_dir(&dir).expect("classpath links");

        let shape = reg.by_name("Shape").expect("Shape loaded");
        let square = reg.by_name("Square").expect("Square loaded");
        let rect = reg.by_name("Rect").expect("Rect loaded");

        // Subclass layout extends the superclass's: `size` keeps its slot, `height` follows it.
        let sh = reg.get(shape).unwrap();
        let rc = reg.get(rect).unwrap();
        assert_eq!(sh.field_slot("size", "I"), Some(0));
        assert_eq!(
            rc.field_slot("size", "I"),
            Some(0),
            "inherited slot is stable"
        );
        assert_eq!(rc.field_slot("height", "I"), Some(1));
        assert_eq!(rc.field_base, 1, "Rect's own fields start after Shape's");
        assert_eq!(rc.field_defaults.len(), 2);

        // Dispatch: `area` occupies one slot, and each subclass overrides it in place (§13.4).
        let slot = sh.vslot("area", "()I").expect("area is virtual");
        assert_eq!(sh.vtable[slot].class, shape);
        assert_eq!(reg.get(square).unwrap().vtable[slot].class, square);
        assert_eq!(rc.vtable[slot].class, rect);
        // `doubled` is inherited, not overridden: both subclasses still point at Shape's method.
        let d = sh.vslot("doubled", "()I").expect("doubled is virtual");
        assert_eq!(reg.get(square).unwrap().vtable[d].class, shape);

        // Constructors and static methods are not virtually dispatched.
        assert_eq!(sh.vslot("<init>", "(I)V"), None);
        assert_eq!(sh.vslot("describe", "(LShape;)I"), None);

        // Subtyping, for instanceof/checkcast.
        assert!(reg.is_subclass_of(square, shape));
        assert!(reg.is_subclass_of(square, square));
        assert!(!reg.is_subclass_of(shape, square));
        assert!(!reg.is_subclass_of(square, rect));
    }

    #[test]
    fn statics_start_at_their_default_and_init_is_lazy() {
        let Some(dir) = compile(&["Shape", "Square", "Rect", "Shapes"], "statics") else {
            eprintln!("skipping loader statics test: javac unavailable");
            return;
        };
        let mut reg = ClassRegistry::new();
        reg.load_dir(&dir).unwrap();
        let shapes = reg.get(reg.by_name("Shapes").unwrap()).unwrap();

        // Static storage exists with JVMS defaults; `<clinit>` has NOT run yet (§8.5).
        let counter = shapes.static_slot("counter", "I").expect("counter");
        let base = shapes.static_slot("BASE", "I").expect("BASE");
        assert_eq!(shapes.statics.borrow()[counter].as_i32(), 0);
        assert_eq!(shapes.statics.borrow()[base].as_i32(), 0);
        assert_eq!(shapes.init_state(), InitState::Loaded);
    }
}
