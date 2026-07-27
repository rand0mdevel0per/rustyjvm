//! rjava-classfile — class-file parsing (RJVM-SPEC-001 §3.2): magic/version, constant pool,
//! fields, methods, and attributes (fully decoding the `Code` attribute). This is the
//! well-formedness/integrity front end (§7.1); JVMS §4.10 *verification* is a separate concern
//! handled by `rjava-verify`. Parsing never panics on adversarial input — every read is
//! bounds-checked (§4.3, §23.3).

mod attribute;
mod constant_pool;
mod error;
mod reader;

pub use attribute::{parse_attributes, Attribute, CodeAttr, ExceptionTableEntry};
pub use constant_pool::{Constant, ConstantPool};
pub use error::ClassFileError;
pub use reader::Reader;

/// A parsed `field_info` or `method_info` (JVMS §4.5, §4.6 — identical shape).
#[derive(Debug, Clone)]
pub struct MemberInfo {
    pub access_flags: u16,
    pub name_index: u16,
    pub descriptor_index: u16,
    pub attributes: Vec<Attribute>,
}

impl MemberInfo {
    fn parse(r: &mut Reader, cp: &ConstantPool) -> Result<MemberInfo, ClassFileError> {
        let access_flags = r.u2()?;
        let name_index = r.u2()?;
        let descriptor_index = r.u2()?;
        let attributes = parse_attributes(r, cp)?;
        Ok(MemberInfo {
            access_flags,
            name_index,
            descriptor_index,
            attributes,
        })
    }

    /// The member's name, resolved against the constant pool.
    pub fn name<'a>(&self, cp: &'a ConstantPool) -> Option<&'a str> {
        cp.utf8(self.name_index)
    }

    /// The member's type descriptor, resolved against the constant pool.
    pub fn descriptor<'a>(&self, cp: &'a ConstantPool) -> Option<&'a str> {
        cp.utf8(self.descriptor_index)
    }

    /// The member's `Code` attribute, if any (methods only).
    pub fn code(&self) -> Option<&CodeAttr> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::Code(c) => Some(c),
            _ => None,
        })
    }
}

/// A parsed class file (JVMS §4.1).
#[derive(Debug, Clone)]
pub struct ClassFile {
    pub minor: u16,
    pub major: u16,
    pub constant_pool: ConstantPool,
    pub access_flags: u16,
    pub this_class: u16,
    pub super_class: u16,
    pub interfaces: Vec<u16>,
    pub fields: Vec<MemberInfo>,
    pub methods: Vec<MemberInfo>,
    pub attributes: Vec<Attribute>,
}

impl ClassFile {
    /// This class's internal name (e.g. `Slice`, `java/lang/Object`).
    pub fn this_class_name(&self) -> Option<&str> {
        self.constant_pool.class_name(self.this_class)
    }

    /// The superclass's internal name, or `None` for `java/lang/Object` (whose `super_class` is 0).
    pub fn super_class_name(&self) -> Option<&str> {
        if self.super_class == 0 {
            None
        } else {
            self.constant_pool.class_name(self.super_class)
        }
    }

    /// Find a method by name and descriptor.
    pub fn method(&self, name: &str, descriptor: &str) -> Option<&MemberInfo> {
        self.methods.iter().find(|m| {
            m.name(&self.constant_pool) == Some(name)
                && m.descriptor(&self.constant_pool) == Some(descriptor)
        })
    }
}

fn parse_members(r: &mut Reader, cp: &ConstantPool) -> Result<Vec<MemberInfo>, ClassFileError> {
    let count = r.u2()?;
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        out.push(MemberInfo::parse(r, cp)?);
    }
    Ok(out)
}

/// Parse a class file from its raw bytes (JVMS §4.1).
pub fn parse(data: &[u8]) -> Result<ClassFile, ClassFileError> {
    let mut r = Reader::new(data);
    let magic = r.u4()?;
    if magic != 0xCAFE_BABE {
        return Err(ClassFileError::BadMagic(magic));
    }
    let minor = r.u2()?;
    let major = r.u2()?;
    let constant_pool = ConstantPool::parse(&mut r)?;
    let access_flags = r.u2()?;
    let this_class = r.u2()?;
    let super_class = r.u2()?;
    let interfaces_count = r.u2()?;
    let mut interfaces = Vec::with_capacity(interfaces_count as usize);
    for _ in 0..interfaces_count {
        interfaces.push(r.u2()?);
    }
    let fields = parse_members(&mut r, &constant_pool)?;
    let methods = parse_members(&mut r, &constant_pool)?;
    let attributes = parse_attributes(&mut r, &constant_pool)?;
    Ok(ClassFile {
        minor,
        major,
        constant_pool,
        access_flags,
        this_class,
        super_class,
        interfaces,
        fields,
        methods,
        attributes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile `testdata/java/Slice.java` with `javac` if available, returning the class bytes.
    /// Returns `None` (test skips) when `javac` is not on PATH — e.g. the CI `build-test` job,
    /// which has no JDK; the `differential` job (Corretto 21) exercises this path.
    fn compile_slice() -> Option<Vec<u8>> {
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/java/Slice.java");
        if !src.exists() {
            return None;
        }
        let out = std::env::temp_dir().join(format!("rjava-cf-{}", std::process::id()));
        std::fs::create_dir_all(&out).ok()?;
        let status = std::process::Command::new("javac")
            .args(["--release", "21", "-d"])
            .arg(&out)
            .arg(&src)
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        std::fs::read(out.join("Slice.class")).ok()
    }

    #[test]
    fn parses_real_slice_class() {
        let Some(bytes) = compile_slice() else {
            eprintln!("skipping parses_real_slice_class: javac unavailable");
            return;
        };
        let cf = parse(&bytes).expect("Slice.class should parse");
        assert_eq!(cf.major, 65, "Java 21 classfile major version");
        assert_eq!(cf.this_class_name(), Some("Slice"));
        assert_eq!(cf.super_class_name(), Some("java/lang/Object"));
        assert_eq!(cf.methods.len(), 2); // <init> and arith
        assert!(cf.fields.is_empty());

        let arith = cf.method("arith", "(IIJF)I").expect("arith(IIJF)I present");
        assert_ne!(arith.access_flags & 0x0008, 0, "ACC_STATIC");
        let code = arith.code().expect("Code attribute");
        assert_eq!(code.max_stack, 4);
        assert_eq!(code.max_locals, 9);
        assert_eq!(code.code.first(), Some(&0x1a)); // iload_0
        assert_eq!(code.code.last(), Some(&0xac)); // ireturn
        assert!(code.exception_table.is_empty());
        // The verifier will decode this; here we only confirm it survived parsing as a raw attr.
        assert!(
            code.attributes
                .iter()
                .any(|a| a.raw_named(&cf.constant_pool, "StackMapTable").is_some()),
            "StackMapTable retained in Code attributes"
        );
    }

    #[test]
    fn rejects_bad_magic() {
        assert_eq!(
            parse(&[0, 0, 0, 0]).unwrap_err(),
            ClassFileError::BadMagic(0)
        );
        // Truncated input is an Eof error, never a panic.
        assert!(matches!(
            parse(&[0xCA, 0xFE]).unwrap_err(),
            ClassFileError::Eof(_)
        ));
    }
}
