//! rjava-std — the Java standard library implemented in Rust (RJVM-SPEC-001 §18).
//!
//! Increment 7 provides the genesis classes guest code cannot do without: `java.lang.Object`,
//! `java.lang.String`, `java.lang.System` and `java.io.PrintStream`. Each is a [`BuiltinClass`] —
//! data handed to the class registry, with method bodies as plain function pointers — so nothing
//! here is linked into the interpreter and the dependency direction of §3.2 is preserved.
//!
//! Behaviour must match the Corretto conformance base observably, **including quirks guests rely
//! on** (§18.2): `String.hashCode` is the JLS-specified `31·h + c` polynomial, not "any hash".

use rjava_core::{BuiltinClass, BuiltinMethod, NativeEnv, NativeError, Val128};

/// `java.lang.Object`: the root of the hierarchy. Its superclass is `null` in a conformant JVM
/// (§17.2), which is why `super_name` is `None`.
fn object_class() -> BuiltinClass {
    BuiltinClass {
        name: "java/lang/Object",
        super_name: None,
        fields: &[],
        statics: &[],
        methods: vec![
            BuiltinMethod {
                name: "<init>",
                descriptor: "()V",
                is_static: false,
                body: |_env, _args| Ok(None), // no observable effect
            },
            BuiltinMethod {
                name: "hashCode",
                descriptor: "()I",
                is_static: false,
                // Identity hash: the registry index is stable for an object's lifetime, which is
                // exactly the contract (`equal objects have equal hashes` holds trivially for
                // identity equality). The *value* is unspecified, so it needs no oracle match.
                body: |_env, args| {
                    let this = *args.first().ok_or(NativeError::BadValue)?;
                    if !this.tag().is_ref() {
                        return Err(NativeError::NullPointer);
                    }
                    Ok(Some(Val128::from_i32(this.ref_index().0 as i32)))
                },
            },
            BuiltinMethod {
                name: "equals",
                descriptor: "(Ljava/lang/Object;)Z",
                is_static: false,
                body: |_env, args| {
                    let a = *args.first().ok_or(NativeError::BadValue)?;
                    let b = *args.get(1).ok_or(NativeError::BadValue)?;
                    // Reference equality, per Object.equals.
                    let same =
                        a.tag().is_ref() && b.tag().is_ref() && a.ref_index() == b.ref_index();
                    Ok(Some(Val128::from_i32(same as i32)))
                },
            },
        ],
    }
}

/// The receiver's text, or a null-pointer error.
fn this_text(env: &dyn NativeEnv, args: &[Val128]) -> Result<String, NativeError> {
    let this = *args.first().ok_or(NativeError::BadValue)?;
    if this.tag() == rjava_core::Tag::Null {
        return Err(NativeError::NullPointer);
    }
    env.string_text(this).ok_or(NativeError::BadValue)
}

/// `java.lang.String`. Its characters live in the object's native payload until array objects
/// exist; a real JVM keeps them in a `byte[]` field, which is where they will move.
fn string_class() -> BuiltinClass {
    BuiltinClass {
        name: "java/lang/String",
        super_name: Some("java/lang/Object"),
        fields: &[],
        statics: &[],
        methods: vec![
            BuiltinMethod {
                name: "length",
                descriptor: "()I",
                is_static: false,
                // JLS counts UTF-16 code units, not Unicode scalar values: "😀".length() == 2.
                body: |env, args| {
                    let s = this_text(env, args)?;
                    Ok(Some(Val128::from_i32(s.encode_utf16().count() as i32)))
                },
            },
            BuiltinMethod {
                name: "charAt",
                descriptor: "(I)C",
                is_static: false,
                body: |env, args| {
                    let s = this_text(env, args)?;
                    let i = args.get(1).ok_or(NativeError::BadValue)?.as_i32();
                    let units: Vec<u16> = s.encode_utf16().collect();
                    if i < 0 || i as usize >= units.len() {
                        // StringIndexOutOfBoundsException once exceptions exist (increment 8).
                        return Err(NativeError::BadValue);
                    }
                    Ok(Some(Val128::from_i32(units[i as usize] as i32)))
                },
            },
            BuiltinMethod {
                name: "isEmpty",
                descriptor: "()Z",
                is_static: false,
                body: |env, args| {
                    let s = this_text(env, args)?;
                    Ok(Some(Val128::from_i32(s.is_empty() as i32)))
                },
            },
            BuiltinMethod {
                name: "hashCode",
                descriptor: "()I",
                is_static: false,
                // JLS 12.4.1: s[0]*31^(n-1) + s[1]*31^(n-2) + ... , over UTF-16 code units, with
                // int wraparound. Guests (and HashMap layouts) depend on the exact value.
                body: |env, args| {
                    let s = this_text(env, args)?;
                    let mut h: i32 = 0;
                    for u in s.encode_utf16() {
                        h = h.wrapping_mul(31).wrapping_add(u as i32);
                    }
                    Ok(Some(Val128::from_i32(h)))
                },
            },
            BuiltinMethod {
                name: "equals",
                descriptor: "(Ljava/lang/Object;)Z",
                is_static: false,
                body: |env, args| {
                    let a = this_text(env, args)?;
                    let other = *args.get(1).ok_or(NativeError::BadValue)?;
                    // Only equal to another String with the same characters.
                    let eq = env.string_text(other).is_some_and(|b| a == b);
                    Ok(Some(Val128::from_i32(eq as i32)))
                },
            },
            BuiltinMethod {
                name: "concat",
                descriptor: "(Ljava/lang/String;)Ljava/lang/String;",
                is_static: false,
                body: |env, args| {
                    let a = this_text(env, args)?;
                    let other = *args.get(1).ok_or(NativeError::BadValue)?;
                    let b = env.string_text(other).ok_or(NativeError::NullPointer)?;
                    let joined = format!("{a}{b}");
                    Ok(Some(env.new_string(&joined)?))
                },
            },
            BuiltinMethod {
                name: "toString",
                descriptor: "()Ljava/lang/String;",
                is_static: false,
                body: |_env, args| Ok(Some(*args.first().ok_or(NativeError::BadValue)?)),
            },
        ],
    }
}

/// `java.io.PrintStream` — enough of it for `System.out`.
fn print_stream_class() -> BuiltinClass {
    fn print_line(env: &mut dyn NativeEnv, text: &str) -> Result<Option<Val128>, NativeError> {
        // Java's println terminates with '\n' on every platform for `System.out`'s default
        // encoding here; matching Corretto's byte stream is what the differential test compares.
        env.print(text);
        env.print("\n");
        Ok(None)
    }
    BuiltinClass {
        name: "java/io/PrintStream",
        super_name: Some("java/lang/Object"),
        fields: &[],
        statics: &[],
        methods: vec![
            BuiltinMethod {
                name: "println",
                descriptor: "(Ljava/lang/String;)V",
                is_static: false,
                body: |env, args| {
                    let arg = *args.get(1).ok_or(NativeError::BadValue)?;
                    // `println((String) null)` prints "null", it does not throw.
                    let text = env.string_text(arg).unwrap_or_else(|| "null".to_string());
                    print_line(env, &text)
                },
            },
            BuiltinMethod {
                name: "println",
                descriptor: "(I)V",
                is_static: false,
                body: |env, args| {
                    let v = args.get(1).ok_or(NativeError::BadValue)?.as_i32();
                    print_line(env, &v.to_string())
                },
            },
            BuiltinMethod {
                name: "println",
                descriptor: "(J)V",
                is_static: false,
                body: |env, args| {
                    let v = args.get(1).ok_or(NativeError::BadValue)?.as_i64();
                    print_line(env, &v.to_string())
                },
            },
            BuiltinMethod {
                name: "println",
                descriptor: "(Z)V",
                is_static: false,
                body: |env, args| {
                    let v = args.get(1).ok_or(NativeError::BadValue)?.as_i32();
                    print_line(env, if v != 0 { "true" } else { "false" })
                },
            },
            BuiltinMethod {
                name: "println",
                descriptor: "(C)V",
                is_static: false,
                body: |env, args| {
                    let v = args.get(1).ok_or(NativeError::BadValue)?.as_i32() as u16;
                    let s = String::from_utf16_lossy(&[v]);
                    print_line(env, &s)
                },
            },
            BuiltinMethod {
                name: "print",
                descriptor: "(Ljava/lang/String;)V",
                is_static: false,
                body: |env, args| {
                    let arg = *args.get(1).ok_or(NativeError::BadValue)?;
                    let text = env.string_text(arg).unwrap_or_else(|| "null".to_string());
                    env.print(&text);
                    Ok(None)
                },
            },
            BuiltinMethod {
                name: "print",
                descriptor: "(I)V",
                is_static: false,
                body: |env, args| {
                    let v = args.get(1).ok_or(NativeError::BadValue)?.as_i32();
                    env.print(&v.to_string());
                    Ok(None)
                },
            },
        ],
    }
}

/// `java.lang.System`. `out` is a static field holding the singleton `PrintStream`; the interpreter
/// installs it when the builtins are registered.
fn system_class() -> BuiltinClass {
    BuiltinClass {
        name: "java/lang/System",
        super_name: Some("java/lang/Object"),
        fields: &[],
        statics: &[
            ("out", "Ljava/io/PrintStream;"),
            ("err", "Ljava/io/PrintStream;"),
        ],
        methods: vec![],
    }
}

/// Every builtin class, in dependency order (a superclass precedes its subclasses).
pub fn builtins() -> Vec<BuiltinClass> {
    vec![
        object_class(),
        string_class(),
        print_stream_class(),
        system_class(),
    ]
}
