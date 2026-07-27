\# RJVM-SPEC-001: RustyJVM Architecture and Implementation Specification



| Field            | Value                                                        |

|------------------|--------------------------------------------------------------|

| Document ID      | RJVM-SPEC-001                                                 |

| Title            | RustyJVM Architecture and Implementation Specification       |

| Version          | 1.0.0                                                         |

| Status           | DRAFT — pending secondary agent review                       |

| Category         | Standards Track (Implementation Normative)                   |

| Language          | English (normative)                                          |

| Target Runtime   | RustyJVM (clean-room JVM in Rust)                            |

| Conformance Base | Amazon Corretto 21 (primary), Corretto 17, Corretto 8        |

| Specification Base | JVMS SE 21 (JSR 924 lineage), JLS SE 21, JSR 133 (JMM)     |



\---



\## 0. Notice to Implementers and Reviewers



This document is the \*\*normative constitution\*\* of the RustyJVM project. All engineering

tasks derive from it. Where this document uses the RFC 2119 key words, they carry their

formal meaning (Section 1.2). Sections marked \*\*\[NORMATIVE]\*\* define required behavior;

sections marked \*\*\[RATIONALE]\*\* record \*why\* a decision was made, including rejected

alternatives, and MUST NOT be re-litigated without new evidence. Sections marked

\*\*\[INFORMATIVE]\*\* provide non-binding guidance.



Reviewers: pay special attention to the \*\*\[RATIONALE]\*\* blocks. Several designs in this

document (notably in Sections 6, 8, 10, 11) were reached after explicitly rejecting

plausible-but-unsound alternatives. Those rejections are load-bearing.



\---



\## 1. Introduction



\### 1.1. Scope and Goals



RustyJVM is a clean-room implementation of a Java Virtual Machine in Rust, targeting

production-grade conformance with Amazon Corretto across desktop (Windows/Linux/macOS),

Android, and iOS. It is a \*\*general-purpose JVM\*\*, not a Minecraft-specific runtime;

Minecraft (Java Edition, with modding) is treated as the primary stress and conformance

workload because it exercises the full surface: dynamic class loading, bytecode

transformation, heavy allocation, floating-point determinism, and native graphics.



Primary goals, in priority order:



1\. \*\*Correctness\*\* — observable behavior MUST match the Corretto conformance base

&#x20;  (Section 19). A prod-ready JVM, not a toy.

2\. \*\*Portability\*\* — a single engine core spanning three platform families, with

&#x20;  conditional compilation for platform constraints (notably iOS JIT prohibition).

3\. \*\*Performance\*\* — competitive interpretation and JIT throughput, exploiting Rust's

&#x20;  ownership model to minimize garbage-collection cost where provably safe.

4\. \*\*Reusability\*\* — maximum code sharing across platforms and across supported Java

&#x20;  versions.



\### 1.2. Requirement Levels



The key words \*\*MUST\*\*, \*\*MUST NOT\*\*, \*\*REQUIRED\*\*, \*\*SHALL\*\*, \*\*SHALL NOT\*\*, \*\*SHOULD\*\*,

\*\*SHOULD NOT\*\*, \*\*RECOMMENDED\*\*, \*\*MAY\*\*, and \*\*OPTIONAL\*\* in this document are to be

interpreted as described in RFC 2119 and RFC 8174 when, and only when, they appear in all

capitals.



\### 1.3. Terminology



| Term | Definition |

|------|------------|

| \*\*Guest\*\* | Java code executed by RustyJVM (untrusted unless otherwise stated). |

| \*\*Host\*\* | The Rust process embedding RustyJVM. |

| \*\*Handle (曲柄)\*\* | A move-only, exclusive-ownership 128-bit reference to a heap object. |

| \*\*Pointer (ptr)\*\* | A shared (non-exclusive) 128-bit reference to a heap object. |

| \*\*Registry\*\* | The indirection table mapping reference indices to object storage and lifecycle metadata. |

| \*\*L1 / L2 / L3\*\* | The three IR/execution tiers (Section 9). |

| \*\*rjvml1\*\* | The on-disk, verified, flattened representation produced by L1. |

| \*\*op-stream-feed\*\* | The runtime linear instruction stream consumed by L3 (the L2 IR). |

| \*\*Diff\*\* | A speculative delta over an execution environment snapshot (Section 10). |

| \*\*vt\*\* | Virtual thread; the unit of Java-level concurrency (Section 11). |

| \*\*tmgr\*\* | Thread manager: vt scheduler plus JMM memory-order guard. |

| \*\*S1 / S2 / S3\*\* | The three object escape states (Section 5). |

| \*\*ClassId\*\* | The runtime-unique identity of a loaded class = (name, defining loader). |

| \*\*BSM\*\* | Bootstrap method for `invokedynamic`. |

| \*\*SATB\*\* | Snapshot-at-the-beginning; the concurrent-marking write barrier discipline. |



\### 1.4. Non-Goals



\- RustyJVM does NOT aim to be bit-compatible with HotSpot's \*internal\* implementation

&#x20; (GC layout, JIT internals, thread scheduling). Only \*observable\* behavior is normative.

\- RustyJVM does NOT implement `SecurityManager` (removed per JEP 411/486; Section 19.4).

\- RustyJVM does NOT provide JVMTI/JDWP debugging in v1.0 (deferred, Section 24).



\---



\## 2. Design Principles and Standards \[NORMATIVE]



These principles are the tie-breakers for all subsequent design decisions.



\### 2.1. P-1: The Interpreter Is the Sole Source of Truth



The register interpreter (L3) MUST be a complete, correct executor on its own. JIT and AOT

are \*\*optional acceleration layers\*\* that MUST be removable at any time without affecting

correctness. Any platform (notably iOS) that forbids one acceleration path MUST still run

correctly on the interpreter baseline. Corollary: no correctness-critical logic may live

exclusively in JIT/AOT code paths.



\### 2.2. P-2: Cost Conservation



Reachability is a global, dynamic graph property. Its cost may be paid at \*collection time\*

(tracing), at \*mutation time\* (reference counting / write barriers), or via \*expressiveness

constraints\* (ownership). It cannot be eliminated. RustyJVM MUST NOT claim "zero-overhead

GC" for the general case; it MUST instead maximize the provably-local subset (S1) that

genuinely bears zero GC cost, and pay minimum necessary cost on the remainder.



\### 2.3. P-3: Bake Only What Is Invariant



Any optimization that hardcodes a runtime-variable fact (class identity, dispatch target,

type) into compiled code MUST be gated by a proof of invariance. Where invariance cannot be

proven, the implementation MUST fall back to runtime-resolved indirection (mutable dispatch

tables, symbolic references, `ensure\_init` guards). This principle eliminates the need for

deoptimization (Section 13.4).



\### 2.4. P-4: Memory Safety Is Non-Negotiable; Type Safety Is Verified



Host memory safety MUST hold even for adversarially crafted, CRC-valid bytecode. This is

guaranteed structurally (Section 4: no raw guest pointers, tagged values, bounds-checked

indirection). Guest-level type safety is guaranteed by mandatory bytecode verification

(Section 7), which additionally supplies the static type proofs that let the JIT elide

runtime checks.



\### 2.5. P-5: Observable Conformance to Corretto



For any input, RustyJVM's observable output (return values, thrown exception types and

timing, reflective results, serialized forms, floating-point results) MUST equal that of

the Corretto conformance base for the active Java version. Implementation internals are

free. Verification is by differential testing (Section 23).



\### 2.6. P-6: Single Core, Version-Gated Library



There MUST be exactly one engine core and one `rustystd` source tree. Behavioral

differences between supported Java versions MUST be expressed via runtime version gates,

not by forking the codebase.



\### 2.7. Design Standards (RFC-style conventions)



\- \*\*STD-CODE-1\*\*: All guest-observable heap references MUST be 128-bit tagged values

&#x20; (Section 4). No API may expose a raw host pointer to guest-reachable state.

\- \*\*STD-CODE-2\*\*: All `unsafe` blocks MUST be confined to `rjava-core` and `rjava-std`

&#x20; primitive modules, MUST be individually documented with a safety invariant, and MUST NOT

&#x20; depend on unverified guest input for their soundness.

\- \*\*STD-CODE-3\*\*: Every normative behavior MUST have a corresponding differential test

&#x20; against the conformance base before it is considered complete.

\- \*\*STD-CODE-4\*\*: Public error conditions MUST map to the JVMS-specified exception/error

&#x20; type and MUST be raised at the JVMS-specified time (lazy where lazy is specified).

\- \*\*STD-VER-1\*\*: Semantic versioning applies to this spec and to the runtime ABI between

&#x20; crates.



\---



\## 3. System Architecture Overview \[NORMATIVE]



\### 3.1. Layered Structure



```

&#x20;                   Guest .jar / .class (untrusted)

&#x20;                               │

&#x20;             ┌─────────────────▼─────────────────┐

&#x20;             │  ClassLoader layer + Transform Hook │  (Section 8)

&#x20;             │  (parent delegation, mixin/ASM)     │

&#x20;             └─────────────────┬─────────────────┘

&#x20;                               │ final bytes (post-transform)

&#x20;             ┌─────────────────▼─────────────────┐

&#x20;             │  L1: Verify (JVMS 4.10) + Flatten  │  (Sections 7, 9.2)

&#x20;             │  → SSA data-flow tree → .rjvml1     │

&#x20;             └─────────────────┬─────────────────┘

&#x20;                               │ trusted, verified IR

&#x20;             ┌─────────────────▼─────────────────┐

&#x20;             │  L2: op-stream-feed (runtime)       │  (Section 9.3)

&#x20;             │  opid jump table + accel replacement │

&#x20;             └─────────────────┬─────────────────┘

&#x20;                               │ linear op stream

&#x20;             ┌─────────────────▼─────────────────┐

&#x20;             │  L3: Register interpreter           │  (Section 10)

&#x20;             │  diff+dep speculation, fork envs    │

&#x20;             └──┬──────────────┬──────────────┬───┘

&#x20;                │              │              │

&#x20;         ┌──────▼───┐  ┌───────▼──────┐  ┌────▼─────┐

&#x20;         │ Interp   │  │ JIT (accel)  │  │ AOT      │  (Sections 13, 14)

&#x20;         │ (truth)  │  │ trace-based  │  │ .a static │

&#x20;         └──────────┘  └──────────────┘  └──────────┘

```



Cross-cutting subsystems: Memory model (Section 5), GC (Section 6), Concurrency/JMM/

Locking (Section 11–12), Native bridge \& graphics (Section 16), rustystd (Section 18).



\### 3.2. Crate Structure \[NORMATIVE]



The project MUST be organized as a single Cargo workspace with the following crates:



| Crate | Responsibility | Depends on |

|-------|----------------|------------|

| `rjava-core` | Value representation, handles/pointers, registry, `Env`, IR datatypes, `unsafe` primitives, JMM atomics. \*\*No reverse dependencies.\*\* | (leaf) |

| `rjava-classfile` | Class file parsing, constant pool, attributes, StackMapTable decoding. | `rjava-core` |

| `rjava-verify` | JVMS 4.10 bytecode verifier (type-checking via StackMapTable). | `rjava-core`, `rjava-classfile` |

| `rjava-ir` | L1 SSA construction, flattening, inlining, escape analysis. | `rjava-core`, `rjava-classfile`, `rjava-verify` |

| `rjava-loader` | ClassLoader model, ClassRegistry, transform hook, ensure\_init state machine. | `rjava-core`, `rjava-ir` |

| `rjava-gc` | rc fast path, SATB concurrent cycle collector, escape-state promotion. | `rjava-core` |

| `rjava-interp` | L2 op-stream generation, L3 register interpreter, diff/dep speculation, tmgr. | `rjava-core`, `rjava-ir`, `rjava-gc`, `rjava-loader` |

| `rjava-jit` | Trace JIT (LLVM via `inkwell` and/or Cranelift backends). Conditionally compiled. | `rjava-core`, `rjava-ir`, `rjava-interp` |

| `rjava-aot` | Ahead-of-time compilation to static libraries. | `rjava-core`, `rjava-ir`, `rjava-jit` |

| `rjava-std` | Java standard library in Rust (`java.\*`, native method implementations). Pure executor; \*\*no reverse dependency\*\*; depends on `rjava-core` for structs. | `rjava-core`, `rjava-interp` (for reentry) |

| `rjava-native` | JNI bridge, LWJGL redirection, wgpu graphics backend. | `rjava-core`, `rjava-interp` |

| `rjava` | Top-level embeddable library + CLI (`java`-compatible launcher). | all above |



`rjava-std` and the executor are compiled directly into the workspace binary (statically

linked as normal Rust crates), NOT loaded via `extern`/dlopen.



\---



\## 4. Value Representation and Reference Model \[NORMATIVE]



\### 4.1. The 128-bit Tagged Value



All guest values on the operand stack, in local slots, and in object fields are represented

as a fixed \*\*128-bit tagged value\*\*:



```

&#x20;bit 127 .................................. bit 8 | bit 7 .. bit 0

&#x20;┌──────────────────────────────────────────────┬───────────────┐

&#x20;│            payload (120 bits)                  │  type tag (8) │

&#x20;└──────────────────────────────────────────────┴───────────────┘

```



\- The \*\*type tag\*\* (8 bits) is maintained exclusively by the interpreter and is

&#x20; \*\*immutable by guest code\*\*. It identifies the schema of the payload (e.g., `i32`,

&#x20; `i64`, `f32`, `f64`, `ptr`, `handle`, `null`, `returnAddress`, plus internal kinds).

\- The \*\*payload\*\* (120 bits) holds the value or a reference index (Section 4.3).

\- 120 bits of payload is sufficient for all primitive encodings and for a reference index

&#x20; plus reference-local metadata. Implementations MUST NOT assume more than 120 payload bits.



\*\*Rationale \[RATIONALE]:\*\* A uniform 128-bit representation removes per-slot type branching

in storage layout at the cost of requiring a tag. The tag has two roles: (a) \*schema

dispatch\* — mandatory, the interpreter cannot interpret payload bits without it; (b) \*type

safety\* — checking `tag == ptr` before dereference prevents type confusion. Role (a) cannot

be removed. Role (b) can be statically discharged by verification and thus elided in JIT'd

code (Section 13.5). A 64-bit representation was rejected: after 8 tag bits, 56 bits cannot

hold a reference index plus the required reference-local metadata.



\### 4.2. Handles and Pointers



Both handles (曲柄) and pointers (ptr) are 128-bit tagged values distinguished only by tag:



\- \*\*Handle\*\* — carries \*\*exclusive (move) ownership\*\* semantics. A handle MUST NOT be

&#x20; duplicated; transferring it invalidates the source. Used for S1 objects (Section 5.1).

\- \*\*Pointer\*\* — carries \*\*shared\*\* semantics; may be aliased. Used for S2/S3.



A handle MAY be converted to a pointer for use; the two are bit-length identical and occupy

the same slot layout.



\### 4.3. Indirection via Registry



Guest reference values MUST NOT be raw host addresses. The payload of a `ptr`/`handle` is

an \*\*index\*\* (plus reference-local bits) into a lifecycle-managed store. Dereferencing MUST:



1\. Verify `tag ∈ {ptr, handle}`, else raise `TypeInvalid` (internal), surfaced as the

&#x20;  JVMS-appropriate condition.

2\. Bounds-check the index against the registry, else raise the internal invalid-access

&#x20;  condition (this MUST be impossible for verified code; it is defense-in-depth).

3\. Resolve to object storage through the registry.



\*\*Memory safety corollary:\*\* Because guest code cannot fabricate a `ptr` tag (tag bits are

interpreter-controlled) and cannot address outside the registry index space, no guest

input — however malicious — can cause the host to dereference arbitrary memory. This holds

\*independently\* of verification. Verification additionally prevents guest-level type

confusion (a distinct property; see Section 7.1).



\### 4.4. Lifecycle Metadata Placement \[NORMATIVE]



For \*\*S3\*\* objects, reference counts and validity flags MUST live in a \*\*separate registry

control block\*\* (analogous to an `Arc` control block), NOT inline in the copied 128-bit

pointer value. Consequences:



\- The copied pointer value is \*\*immutable\*\*; concurrent 128-bit copies never tear on

&#x20; metadata mutation.

\- Validity is checked by an indirection to the control block.

\- `strong == 0` destroys the object payload (runs finalization, Section 6.6); the control

&#x20; block persists so weak references can observe "dead"; the entry is freed only when

&#x20; `strong == 0 ∧ weak == 0`.



For \*\*S1\*\* objects, no control block exists (Section 5.1). For \*\*S2\*\*, a lightweight

non-atomic counter is used (Section 5.2).



\*\*Rationale \[RATIONALE]:\*\* Placing a mutable validity bit inside the copied pointer would

require a 128-bit atomic (LDXP/STXP or CASP) on every copy and a torn-read hazard when the

object flips it. Separating mutable metadata into the control block makes the pointer value

a pure immutable datum. See Section 12.6 for the ARM64 128-bit atomicity constraints that

motivate this.



\---



\## 5. Memory Model: The Three Escape States \[NORMATIVE]



Every heap object exists in exactly one of three states at any time. State is determined

by escape analysis (Section 9.4) and promoted at runtime. The state governs which

concurrency and reclamation machinery applies.



\### 5.1. S1 — Scope-Exclusive



\- \*\*Criteria:\*\* the object is provably not aliased, not stored into any longer-lived

&#x20; object, and not returned across its defining scope; escape analysis proves non-escape.

\- \*\*Representation:\*\* a \*\*handle\*\* (曲柄). No reference count. No validity flag. No

&#x20; back-set.

\- \*\*Reclamation:\*\* RAII — destroyed deterministically at scope exit (`Env` drop).

\- \*\*Concurrency:\*\* none; single owner within a logically serial diff chain. Free to

&#x20; speculate; never locked.

\- \*\*Cost:\*\* genuinely \*\*zero GC cost\*\*. This state MUST be maximized (P-2).



\### 5.2. S2 — Thread-Local Shared



\- \*\*Criteria:\*\* aliased within a single thread, or stored into a same-thread longer-lived

&#x20; object, or returned to the caller, but never observed by a second vt.

\- \*\*Representation:\*\* `unsharedptr` — a pointer backed by a \*\*non-atomic\*\* reference count.

&#x20; No back-set. No lock. No control-block atomics.

\- \*\*Reclamation:\*\* reference counting; `rc == 0` reclaims (subject to cycle handling,

&#x20; Section 6).

\- \*\*Concurrency:\*\* intra-vt only. Reference-count adjustments are \*\*metadata\*\*, not data

&#x20; dependencies, and MUST be applied at diff-landing time in program order (Section 5.5).

\- \*\*Cost:\*\* minimal (one non-atomic counter).



\### 5.3. S3 — Cross-Thread Shared



\- \*\*Criteria:\*\* stored into an already-S3 object's field, passed to another vt, or the

&#x20; variable enters a scope active on ≥2 distinct vt ids ("shared variable", Section 11.5).

\- \*\*Representation:\*\* pointer backed by a registry \*\*control block\*\* with \*\*CASP-atomic\*\*

&#x20; reference counts, a \*\*back-set\*\* (for weak-reference reverse notification), and a

&#x20; read/write lock (u16, upgraded to u32 for hot objects, Section 12.7).

\- \*\*Reclamation:\*\* atomic reference counting + participation in the concurrent cycle

&#x20; collector (Section 6).

\- \*\*Concurrency:\*\* field reads/writes bypass the diff/speculation machinery and go directly

&#x20; through the lock (Section 11.4). NOT forked into speculative snapshots (Section 10.4).

\- \*\*Cost:\*\* highest; reserved for the minority of genuinely shared, long-lived objects.



\### 5.4. Promotion \[NORMATIVE]



Promotion is monotonic (S1 → S2 → S3; never demoted).



\- \*\*S1 → S2:\*\* occurs when the object is aliased or stored into a same-thread longer-lived

&#x20; object. No memory barrier required (still single-threaded). Allocates the non-atomic

&#x20; counter.

\- \*\*S2 → S3:\*\* occurs at the \*\*escape/publish point\*\* — when the object first becomes

&#x20; reachable by a second vt. This point MUST:

&#x20; 1. Commit (land) all speculative diffs referencing the object (Section 10).

&#x20; 2. Install the control block, converting the non-atomic count to CASP-atomic.

&#x20; 3. Emit a \*\*publish (release) fence\*\* so that all prior writes to the object are visible

&#x20;    to any vt that subsequently acquires the reference (JMM safe-publication; also serves

&#x20;    as the `final`-field freeze, Section 12.5).

&#x20; 4. Upgrade the object's lock to the S3 lock (u32 for hot objects).



Promotion detection reuses the escape/shared-variable analysis already required for

scope/id assignment (Section 9); no additional pass is introduced.



\### 5.5. Reference-Count Discipline (S2/S3) \[NORMATIVE]



This is a load-bearing invariant. Reference counts MUST be treated as \*\*metadata, not as a

data dependency\*\* for the purpose of speculation and chain merging:



\- rc mutations MUST NOT cause two otherwise-independent diff chains to be merged. (Merging

&#x20; on rc would serialize all chains touching a hot object, destroying parallelism.)

\- rc increments/decrements MUST be recorded within the diff and applied \*\*serially in

&#x20; program order at diff-landing time\*\*, never eagerly during speculative execution.

\- Actual \*\*data\*\* conflicts on an object's fields (two chains writing the same field) ARE

&#x20; dependencies and MUST trigger chain merging → serialization (Section 10.5).



\*\*Rationale \[RATIONALE]:\*\* Because parallel diff chains fork their environments (Section

10.4), and because rc changes land in program order, non-atomic S2 reference counting is

race-free without locking. Deferred landing also naturally defers reclamation, which is

already the model. Violating this invariant (mutating rc eagerly during speculation) is a

silent memory-corruption source.



\---



\## 6. Garbage Collection \[NORMATIVE]



\### 6.1. Honest Characterization



RustyJVM's reclamation is \*\*reference counting on the fast path plus a concurrent

mark-sweep cycle collector\*\*. It is NOT "no GC." Documentation, papers, and code comments

MUST characterize it accurately (P-2). Claiming the absence of tracing would fail under any

long-running workload that accumulates strong cycles (e.g., Minecraft over hours).



\*\*Rationale \[RATIONALE]:\*\* Java permits arbitrary strong reference cycles and does not

require ownership annotations, so pure reference counting leaks cycles. Two alternatives

were evaluated and \*\*rejected\*\*:



\- \*\*Brownbridge strong/weak pointer cycle elimination\*\* (auto-demoting the cycle-closing

&#x20; edge to weak): \*unsound\*. Edge classification at creation time does not track dynamic

&#x20; reachability; a demoted edge can later become the sole path to a live object, producing a

&#x20; dangling reference (use-after-free). Repairing it (re-strengthening on delete) reintroduces

&#x20; per-mutation graph search, which is costlier than tracing. The VM also must not

&#x20; unilaterally demote a guest's ordinary strong field to weak, as guests do not null-check

&#x20; ordinary references.

\- \*\*Per-node "outer-world reachability" caching with invalidate-on-rc-change:\*\* livelocks

&#x20; under high mutation rates (Minecraft mutates references millions of times per second; a

&#x20; full analysis never completes before invalidation) and had a direction error (it tested

&#x20; "N can reach a root," i.e., liveness must be "a root can reach N").



The retained design below is the sound resolution.



\### 6.2. Reference-Count Fast Path



Acyclic garbage MUST be reclaimed immediately by reference counting (S1 via RAII, S2/S3 via

count-to-zero). This handles the overwhelming majority of objects (generational hypothesis:

most objects die young and never cycle).



\### 6.3. Concurrent Cycle Collector



Strong cycles are collected by a \*\*concurrent mark-sweep\*\* collector:



\- \*\*Roots\*\* MUST include: all vt stacks (locals + operand stacks), all initialized classes'

&#x20; static fields, active monitors, interned strings, JNI global references, and \*\*all live

&#x20; forked diff-environment snapshots\*\* (Section 10.4). Omitting fork snapshots would allow

&#x20; reclamation of objects in active speculative use.

\- \*\*Marking\*\* proceeds \*\*forward from all roots\*\* (mark-reachable), NOT via per-node reverse

&#x20; traversal. (Reverse traversal was rejected: it requires a back-set on every object,

&#x20; including S1/S2 which have none, and is error-prone with respect to direction.)

\- \*\*Concurrency correctness\*\* MUST be maintained by a \*\*SATB (snapshot-at-the-beginning)

&#x20; write barrier\*\*: when a reference field is overwritten, the \*old\* referent is pushed onto

&#x20; the mark queue. This preserves the tri-color invariant under concurrent mutation \*\*without

&#x20; restarting\*\* the marking phase.

\- \*\*Triggering\*\* MUST be by \*\*memory-pressure / allocation watermark\*\*, NOT by rc change.

&#x20; Because the rc fast path reclaims almost everything immediately, cycles accumulate slowly

&#x20; and tracing is rare; its amortized cost is low.

\- \*\*Sweep\*\* marks unreached objects invalid (invalidating all references via the control

&#x20; block) and reclaims them.



\### 6.4. Stop-the-World Bounds



The collector SHOULD be mostly concurrent. Any STW pause (e.g., a short root-scan/handshake)

MUST be bounded and documented. On mobile, STW pauses affecting frame rate MUST be

minimized; the implementation SHOULD prefer incremental/concurrent marking with brief

per-vt safepoint handshakes.



\### 6.5. Write Barrier Integration



The SATB GC write barrier extends the existing barrier infrastructure used for

"non-intrinsic" side-effecting operations (Section 10.6). Reference-field stores MUST route

through the barrier when the referring object is subject to tracing (S3, and S2 objects

reachable from S3).



\### 6.6. Finalization and Reference Objects



\- `finalize()`, `java.lang.ref.Cleaner`, and `Reference`/`ReferenceQueue` MUST be

&#x20; implemented with Corretto-conformant timing.

\- Finalization runs on `strong == 0` of a registry control block, dispatched to a dedicated

&#x20; finalizer vt.

\- `WeakReference`, `SoftReference`, `PhantomReference` reachability semantics MUST be

&#x20; defined against the tracing collector's reachability, not against raw reference counts.

&#x20; Weak references use the object's two-valued (strong/weak) marking and reverse notification

&#x20; via the back-set (S3 only; weak references force at least S2, and weak-referenced objects

&#x20; participate as needed).

\- `SoftReference` clearing policy MUST be memory-pressure driven and MUST be

&#x20; Corretto-comparable in observable aggressiveness.



\### 6.7. Guest-Directed Weak References \[NORMATIVE]



The correct place for a guest to break a cycle is an explicit `java.lang.ref.WeakReference`

(or a weak collection). RustyJVM MUST implement Java weak-reference semantics faithfully;

the guest — which knows ownership direction and null-checks weak referents — selects which

edge is weak. The VM MUST NOT infer this automatically (see 6.1 rejection).



\---



\## 7. Class File Verification \[NORMATIVE]



\### 7.1. Mandatory Verification



Bytecode verification per JVMS §4.10 MUST be performed by default on all untrusted class

files, at L1, \*\*before\*\* flattening/inlining. Verification is a \*\*security floor\*\*, not an

optimization.



\*\*Two orthogonal properties MUST be distinguished:\*\*



\- \*\*Integrity\*\* (CRC32 / signatures): detects corruption or tampering of bytes. CRC32 is

&#x20; non-cryptographic and detects only accidental corruption; it is used for cache validation

&#x20; (Section 8.7) within the sandboxed private directory. It does NOT substitute for

&#x20; verification.

\- \*\*Verification\*\* (JVMS 4.10): ensures that \*well-formed, integrity-valid\* bytecode is

&#x20; semantically safe (no operand stack under/overflow, no jumps into instruction middles, no

&#x20; type confusion, no use of uninitialized objects, correct return types).



CRC32 provides \*\*no\*\* protection against a maliciously crafted but well-formed class file.

The typical hostile scenario is a mod loader executing third-party JARs of unknown

provenance; the VM, not the caller, bears the verification obligation.



\### 7.2. Verification Algorithm



For class files version ≥ 50 (Java 6+), verification MUST use the \*\*type-checking verifier\*\*

driven by `StackMapTable` (linear-time), not the legacy iterative type-inference verifier.

The verifier MUST reject with `VerifyError` on any violation.



\### 7.3. Trust Boundary and Caching



\- Untrusted `.jar`/`.class` → verified once at L1 → the resulting `.rjvml1` artifact is

&#x20; \*\*trusted\*\* thereafter.

\- `rustystd` (bootstrap namespace, compiled into the binary) is trusted and \*\*exempt\*\* from

&#x20; verification.

\- Mods are verified like any untrusted input. (No permission sandbox is layered on top;

&#x20; `SecurityManager` is not implemented per Section 19.4. Isolation is via classloader

&#x20; namespaces and the OS process sandbox.)



\### 7.4. `-Xverify:none`



Disabling verification MUST be an explicit, non-default option, permitted only for fully

trusted inputs (e.g., self-produced AOT-compiled code). It MUST NOT be the default and MUST

NOT be applied to mod/untrusted code.



\### 7.5. Post-Verification Trust in the Engine



Because L1 verification supplies static type information, the JIT MAY safely elide the

runtime tag/type checks that the interpreter performs, within regions proven safe (Section

13.5). The interpreter retains its tag checks as defense-in-depth (P-1, P-4).



\---



\## 8. ClassLoader Model and Class Registry \[NORMATIVE]



\### 8.1. Class Identity



A loaded class's runtime identity is the pair \*\*(fully-qualified name, defining

ClassLoader)\*\*, represented by a `ClassId`. The same name loaded by two loaders yields two

distinct classes that are not assignment-compatible and for which `instanceof` is false.



\### 8.2. Parent Delegation



Class loading MUST implement the parent-delegation model: a loader consults its parent

before defining a class itself, ensuring core classes (`java.\*`) are defined by the

bootstrap namespace and cannot be shadowed by application/mod classes. Loaders MAY be

`parallelCapable`; the registry MUST support concurrent `loadClass`.



\### 8.3. Loading Phases



Loading, linking (verification + preparation + \*\*lazy\*\* resolution), and initialization

(`<clinit>`, on first active use) MUST be distinct and lazily triggered per JVMS §5. Only

verification (a linking sub-step) may be performed ahead of first use; identity and timing

of loading, resolution, and initialization are runtime-determined and MUST NOT be baked at

L1.



\### 8.4. ClassRegistry



```rust

struct ClassId(u32);



struct ClassRegistry {

&#x20;   // Key MUST be (loader\_id, interned\_name) — the basis of namespace isolation.

&#x20;   by\_name:    ConcurrentMap<(LoaderId, InternedName), ClassId>,

&#x20;   classes:    RegistryVec<ClassId, LoadedClass>,   // verified .rjvml1 representation

&#x20;   init\_state: RegistryVec<ClassId, InitState>,

}



enum InitState {

&#x20;   Loaded,                 // verified, not initialized

&#x20;   Initializing(VtId),     // a vt is running <clinit>; enables cyclic-clinit pass-through

&#x20;   Initialized,

&#x20;   Failed,                 // <clinit> threw → subsequent use raises NoClassDefFoundError

}

```



\- `by\_name` MUST be concurrency-safe. Defining the same `(loader, name)` twice MUST be

&#x20; prevented by CAS on definition; losers adopt the winner's result.

\- Cache artifacts (`.rjvml1`) MUST be keyed including loader identity \*\*and\*\* a hash of the

&#x20; post-transform bytes (Section 8.6).



\### 8.5. Lazy Initialization State Machine



`ensure\_init` MUST implement JVMS §5.5 semantics, including recursive superclass

initialization, cyclic-`<clinit>` pass-through (a thread already initializing a class sees

the partially-initialized state without re-entering or deadlocking), multi-thread

serialization, and error wrapping:



```rust

fn ensure\_init(reg: \&ClassRegistry, cid: ClassId, cur\_vt: VtId) {

&#x20;   loop {

&#x20;       match reg.init\_state.get(cid) {

&#x20;           Initialized              => return,

&#x20;           Failed                   => throw(NoClassDefFoundError),

&#x20;           Initializing(v) if v == cur\_vt => return, // same-vt reentry: JVMS 5.5 pass-through

&#x20;           Initializing(\_)          => park\_until\_done(cid),

&#x20;           Loaded => {

&#x20;               if cas\_to\_initializing(cid, cur\_vt) {

&#x20;                   ensure\_init(reg, superclass(cid), cur\_vt); // super first

&#x20;                   match run\_clinit(cid) {                    // may throw

&#x20;                       Ok(())  => set\_initialized(cid),

&#x20;                       Err(e)  => { set\_failed(cid);

&#x20;                                    throw(wrap\_ExceptionInInitializerError(e)); }

&#x20;                   }

&#x20;                   return;

&#x20;               } // lost the race → re-loop

&#x20;           }

&#x20;       }

&#x20;   }

}

```



`<clinit>` MUST NOT be inlined and MUST NOT be executed ahead of its first active-use

trigger. Active-use triggers (JVMS 5.5) are: `new`, static field access (non-constant),

static method invocation, and subclass initialization.



\### 8.6. Bytecode Transformation Hook (mixin / ASM)



Frameworks such as Fabric (`KnotClassLoader`) and Forge (`TransformingClassLoader`)

intercept the byte stream \*before\* `defineClass` and apply mixin/ASM transformations. The

loaded bytecode therefore does not exist on disk; it is synthesized at load time.



RustyJVM MUST model transformation as a \*\*hook chain on the loader\*\*, applied before L1:



```

jar bytes → \[TransformHook chain: mixin/ASM] → transformed bytes

&#x20;         → L1 (verify + flatten) → .rjvml1   (cache key = hash(transformed bytes))

```



\- L1/L2/L3 MUST always operate on the loader's \*\*final delivered bytes\*\*; they MUST NOT

&#x20; trust or re-read the on-disk original after transformation.

\- Transformation complexity is confined to the hook layer and MUST NOT leak into the

&#x20; execution engine.



\### 8.7. Consequences of Transformation



| Concern | Requirement |

|---------|-------------|

| Cache key | MUST include hash of transformed bytes, not merely `(loader, name)`. Mod set changes MUST invalidate. |

| AOT | Mixin-target classes MUST NOT be pre-AOT'd (patches are synthesized at load time). Only classes provably not transformed (e.g., `rustystd`, core libs) may be pre-AOT'd. |

| JIT | On (re)transformation, existing JIT code for the class MUST be invalidated; execution falls back to interpretation, then re-JITs. |

| iOS | Transformation produces bytecode (not machine code) at load time → interpretation is legal and permitted. Such classes can NEVER be AOT'd; on iOS, mixin'd mods run interpreter-only. This is a physical lower bound. |



\### 8.8. Class Unloading



A loader and all classes it defines MUST be reclaimable as a unit when unreachable. With no

tracing-only lifetime, the loader is the \*\*owner\*\* of all classes in its namespace; the

namespace is reclaimed when the loader's reference count reaches zero (participating in the

cycle collector for loader↔class↔instance cycles). This mirrors the scope-ownership model

(a parent scope is not reclaimed while a child scope is live).



\### 8.9. Inlining Gate (Resolution Invariance)



L1 inlining (Section 9.2) is a \*best-effort\* optimization gated by \*\*resolution

invariance\*\*:



\- \*\*Inlinable\*\* iff the target `ClassId` cannot change at runtime (same classloader

&#x20; identity, not shadowable by a child loader) AND the target is monomorphic / final /

&#x20; effectively final.

\- \*\*Not inlinable\*\* (cross-loader, shadowable, or runtime-generated) → retain a symbolic

&#x20; reference resolved lazily via the registry, with `ensure\_init` guards and runtime mutable

&#x20; dispatch (Section 13.4).



`rustystd` (single bootstrap namespace, fixed identity) is the safest inlining/AOT target.



\---



\## 9. Intermediate Representation \[NORMATIVE]



The IR exists in three forms along the pipeline. All three are normative datatypes in

`rjava-core` / `rjava-ir`.



\### 9.1. Value Numbering



SSA value identity MUST use a \*\*two-level\*\* scheme:



```rust

struct ValId {

&#x20;   scope\_level: u16,   // FIXED at compile time (super nesting level); never drifts

&#x20;   offset:      u16,   // VARIABLE; slots may be reordered/reused within a level; ≤ 1024

}

```



\- `scope\_level` is fixed by the super-scope nesting (Section 9.5) and MUST NOT drift across

&#x20; block reordering, providing stable cross-block references.

\- `offset` is reusable within a level (dead SSA values free their offset), providing

&#x20; compactness.

\- Cross-level references cost one extra indirection (locate level, then offset), matching

&#x20; the existing scope-id access pattern.



\*\*Rationale \[RATIONALE]:\*\* Flat global SSA numbering causes id drift across block

transformations and conflicts with the fixed 0..1024 scope-slot model. The two-level scheme

reconciles stability (fixed level) with flexibility (variable offset) and maps directly onto

LLVM/Cranelift virtual registers and `phi` nodes.



\### 9.2. L1 IR — Verified SSA Data-Flow Tree (`.rjvml1`)



Stack-based bytecode MUST be converted to register-based SSA at L1, because L1 def-use edges

are the substrate for out-of-order/early issue (Section 10). L1 is produced \*after\*

verification.



```rust

struct Method {

&#x20;   blocks:     Vec<Block>,        // basic blocks by control flow

&#x20;   entry:      BlockId,

&#x20;   max\_locals: u16,               // ≤ 1024 per scope

&#x20;   exc\_table:  Vec<ExcRegion>,    // exception handler coverage (local catch ranges)

}



struct Block {

&#x20;   id:    BlockId,

&#x20;   phis:  Vec<Phi>,               // control-flow merge points

&#x20;   nodes: Vec<Node>,              // SSA nodes, in program order

&#x20;   term:  Terminator,             // branch / return / throw / switch

}



struct Node {

&#x20;   id:     ValId,                 // SSA def

&#x20;   op:     Op,

&#x20;   ins:    SmallVec<\[ValId; 3]>,  // def-use edges = dependencies

&#x20;   ty:     Tag,                   // static type fixed by verification (feeds JIT)

&#x20;   effect: Effect,

}



enum Effect {

&#x20;   Pure,                          // free to reorder / hoist / speculate

&#x20;   ReadHeap,                      // constrained by store dependencies

&#x20;   WriteHeap,                     // barrier point

&#x20;   Extern,                        // non-intrinsic (native/IO): fence + program-order landing

&#x20;   MayThrow { caught: bool },     // can throw; caught=true installs implicit control edge

}



struct Phi {

&#x20;   slot:    ValId,                          // (this scope\_level, fixed offset) merge slot

&#x20;   sources: SmallVec<\[(BlockId, ValId); 2]>,

}

```



Key semantics:



\- `ins` \*\*is\*\* the dependency set. The scheduler issues a node when its `ins` are ready;

&#x20; "early issue of independent statements" is implemented by inspecting SSA edges, not by a

&#x20; separate analysis.

\- `effect` classification drives speculation eligibility: `Pure` speculates freely;

&#x20; `Extern` is fenced and lands in program order (Section 10.6); `MayThrow { caught: true }`

&#x20; installs an implicit throw control edge; a throwing instruction NOT covered by a local

&#x20; handler is treated without a control edge (an uncaught throw destroys the frame's diffs,

&#x20; Section 20).

\- `ty` is consumed by the JIT to specialize 128-bit tagged values into native unboxed

&#x20; representations (Section 13.5).

\- `phi` is a scope-head merge slot at a fixed offset, not a free-floating node; merging

&#x20; predecessors write the same `(level, offset)`. This maps 1:1 to backend `phi`.



\*\*Inlining\*\* at L1 (P-3, Section 8.9) means: monomorphic method bodies are inlined as

sub-scopes, and imported class references (`sth.classA`) are merged structurally into the

stream at the import site. Inlining is a structural pull-in of definitions; it MUST NOT

resolve symbols eagerly nor run `<clinit>` early (`ensure\_init` guards remain).



\### 9.3. L2 IR — op-stream-feed (runtime)



The L1 tree is \*\*linearized at runtime\*\* into an instruction stream for L3. Not persisted

as such; the on-disk cached artifact is a distinct concept (see naming note below).



```rust

struct OpStream { ops: Vec<L2Op> }



struct L2Op {

&#x20;   opid: u16,        // dispatch = base + opid \* stride → single computed jump

&#x20;   dst:  SlotId,     // result slot (0..1024 within scope)

&#x20;   src:  \[SlotId; 2],

&#x20;   accel: AtomicU32, // 0 = interpret; jit-indicator; compiled-blob-id

}

```



\- `match`/dispatch MUST be compiled to `opid`-indexed computed jumps (`base + opid\*stride`),

&#x20; a single jump, with opid translation done in the highly parallel front end.

\- The `accel` field is the pointer-replacement site: during interpretation it holds an

&#x20; indicator; when async JIT completes, it is CAS'd to a compiled-blob id and the waiter is

&#x20; notified once. This is a single-word CAS and MUST NOT block the interpreter stream.



\*\*Naming \[NORMATIVE]:\*\* To remove the earlier ambiguity, the on-disk cached artifact

(`.rjvml1` + JIT results) MUST be named \*\*`.rjvmc`\*\* (compiled cache). The token "L2" refers

exclusively to the runtime op-stream. The disk artifact MUST NOT be called "l2".



\### 9.4. Escape Analysis



L1 MUST classify each allocation into S1/S2/S3 (Section 5) in a single analysis pass reused

for scope/id assignment:



\- Only-local, not-stored, not-returned → \*\*S1\*\*.

\- Stored into same-thread longer-lived object / multiply-aliased / returned → \*\*S2\*\*.

\- Stored into known-S3 field / passed to another vt / variable enters a cross-tid active

&#x20; scope → \*\*S3\*\*.



Classification is conservative: when non-escape cannot be proven, the object MUST be

promoted to the more general state (never under-classified).



\### 9.5. Scope and Slot Model



\- Variables are stored in a per-scope list indexed by a \*\*sequential id ring\*\*, id range

&#x20; `0..1024`.

\- At L2, variable names are replaced by ids. If a single scope would hold > 1024

&#x20; simultaneous variables, the scope MUST be split: some variables are wrapped in a child

&#x20; scope, and parent-scope variables referenced from the child carry a \*\*`super`\*\* label.

\- `super` labels MAY nest (outer, outer-of-outer, …). A child scope MAY outlive its textual

&#x20; parent when held by a child vt; reclamation of the parent scope's slots MUST wait until

&#x20; all child scopes complete (count-to-zero on scope exit), consistent with Section 8.8.



\### 9.6. L3 Diff Node — Speculative Execution Unit



```rust

struct DiffNode {

&#x20;   tid:  VtId,                     // virtual thread id; trees of different tid never merge

&#x20;   init: EnvSnapshot,              // inherits previous node's result (init + diff)

&#x20;   diff: Vec<(SlotId, Val128)>,    // writes produced by this node

&#x20;   deps: Vec<(SlotId, Val128)>,    // speculative assumptions (value-prediction deps)

&#x20;   po:   u32,                      // program-order landing key

}

```



Semantics detailed in Section 10.



\---



\## 10. Execution Engine (L3) and Speculation \[NORMATIVE]



\### 10.1. Register Interpreter



L3 is a \*\*register-based, pipelined state machine\*\*. Objects are referenced through the

registry via pointers; the lookup/skip-list structures SHOULD be lock-free where feasible.

Independent (dependency-clear) statements are issued early; dependent statements are held in

continuations.



\### 10.2. Worker Dispatch Model



\- Data structures are uniformly `map`-style: a list indexed by sequential id (k → v).

\- Currently-runnable operations (dependencies cleared) MUST be dispatched as worker tasks

&#x20; via `rayon`; their chained sub-operations (continuations) are stored in a lookup table

&#x20; indexed by `workerid`. On worker completion, the continuation is looked up and a successor

&#x20; worker is spawned with the designated `workerid`.

\- Workers that cannot yet run are parked in continuations; scheduling of parked work is the

&#x20; responsibility of the `rayon`/`tokio` carrier layer, NOT of L3. L3 dispatches everything

&#x20; currently runnable; blocked work is carried by the runtime.

\- Task granularity MUST be the \*\*diff chain\*\* (one chain = one task), not individual

&#x20; statements, to amortize spawn overhead.



\### 10.3. Speculation: diff + dep



\- Speculation granularity MUST be the \*\*longest single dependency chain\*\* (adjacent diff

&#x20; layers coalesced), NOT the statement.

\- Speculation is \*\*load-gated\*\*: normal-load speculation rate is very low; speculation is

&#x20; activated only under load gating, on hot regions.

\- Value prediction uses a `deps` (predicted slot values) + `diff` (resulting writes) model.

&#x20; If `deps` hold at execution time, the `diff` is applied and recomputation is skipped

&#x20; (oneshot). If any `dep` is invalidated (predicted value wrong, or a read variable

&#x20; changed), the speculative `diff` is discarded and the chain re-executes.

\- The model is \*\*oneshot\*\* and does \*\*not roll back\*\* committed state. There is no

&#x20; deoptimization (Section 13.4).



\### 10.4. Environment Fork (COW) — S1/S2 only



\- Parallel diff chains \*\*fork\*\* their environments (copy-on-write). Each diff node inherits

&#x20; only its predecessor's result (`init = predecessor(init + diff)`).

\- Forking provides \*\*snapshot isolation\*\*: a chain never observes another chain's unlanded

&#x20; writes.

\- \*\*S3 objects MUST NOT be forked into snapshots.\*\* They remain in the shared heap; all

&#x20; reads/writes bypass the diff machinery and go through the lock (Section 11.4).



\*\*Rationale \[RATIONALE]:\*\* If an S3 object were forked, a write in one vt's snapshot would

be invisible to another vt reading the shared object — breaking cross-thread visibility and

`volatile`. Fork-exclusion of S3 is the foundation of speculation correctness, not merely an

optimization. COW is required; deep-copying the full heap per speculation is infeasible.



\### 10.5. Landing and Ordering



\- Diffs MUST land \*\*incrementally in program order\*\* (as program order advances), NOT in a

&#x20; batch at scope exit. This is what makes intra-vt execution as-if-serial and makes local

&#x20; exception handling correct (Section 20).

\- Within a single vt, po-ordered landing yields single-thread semantics.

\- Cross-chain \*\*data\*\* dependencies (conflicting field access) MUST cause the chains to

&#x20; merge into one serial chain. Because forked environments are disjoint, applying diffs

&#x20; within a vt requires no locking; conflicts are resolved by merging, not locking.

\- Different-tid diff trees MUST NOT merge. They write the same shared `Env`; landing diffs

&#x20; by po into shared/parent state, and reading/writing parent-tree state, uses the S3 lock.



\### 10.6. Barriers



\- "Non-intrinsic" operations — those whose behavior has irreversible external effect or

&#x20; whose logic lies outside the JVM (native, IO) — MUST carry a barrier: they are NOT

&#x20; speculated and land in program order. (`Effect::Extern`.)

\- The GC SATB write barrier (Section 6.5) shares this infrastructure.

\- Fence semantics for JMM (Section 12) are additional barrier obligations.



\### 10.7. Speculation Profitability \[INFORMATIVE / NORMATIVE gate]



Statement-granularity value prediction has historically lost to a fast serial JIT + good GC

on pointer-chasing, polymorphic-heavy workloads (e.g., game main loops). Therefore:



\- The interpreter MUST be correct and reasonably fast \*\*without\*\* speculation (P-1).

\- Speculation MUST be an opt-in, load-gated, hot-region feature (10.3).

\- Implementations MUST instrument mis-speculation rate. If a region's mis-speculation rate

&#x20; exceeds an empirically determined threshold (initial guidance: 30%), speculation for that

&#x20; region MUST be disabled (net-negative on CPU and battery).



\---



\## 11. Concurrency Model and Java Memory Model \[NORMATIVE]



\### 11.1. Virtual Threads



Java-level concurrency MUST use an \*\*M:N virtual-thread\*\* model (Loom-style green threads)

scheduled on a carrier pool. `rayon` and `tokio` serve as the carrier/execution substrate;

they are NOT mapped 1:1 to Java threads. `rayon` provides data-parallel worker dispatch

(Section 10.2); `tokio` MAY provide async carriers for blocking/parked work. Java threads

are NOT mapped directly to `tokio` tasks (blocking Java semantics conflict with cooperative

async).



\*\*Rationale \[RATIONALE]:\*\* All vt run on a shared L3 processor; different vt are

distinguished by tid and by separated diff environments plus a lock. Java's stated

"threads" are, at the engine level, diff trees of different tid processed by the same L3,

each with isolated diff state and lock-mediated access to shared state.



\### 11.2. Thread Manager (tmgr)



`tmgr` is both the vt scheduler and the \*\*JMM memory-order guard\*\*. It is responsible for:



\- vt lifecycle, park/unpark, carrier assignment.

\- Enforcing correct memory-ordering fences on the code L2/L3 generate.

\- Handling `rjavavm.thread`-tagged operations marked at L2 (thread operations are marked at

&#x20; L2 and handled by `tmgr`, not by the general pointer machinery).



\### 11.3. Blocking, wait/notify, Pinning



\- `synchronized`, `Object.wait/notify`, and blocking IO MUST suspend the vt cooperatively,

&#x20; yielding the carrier.

\- When a vt holds a monitor and calls into native code (pinning), it MUST NOT yield; the

&#x20; carrier remains occupied for the pinned duration. Implementations SHOULD document pinning

&#x20; scenarios and their carrier-pool implications.



\### 11.4. Shared Object Access



S3 objects' field reads/writes MUST bypass speculation/diff and go directly through the

object's lock (Section 12). This is the sole locking path; intra-vt (S1/S2, forked) access

requires no locking (Section 10.5).



\### 11.5. Shared-Variable Definition



A variable is \*\*shared\*\* iff its enclosing scope contains sub-scopes on ≥2 distinct tids and

at least two of those sub-scopes are simultaneously \*\*active\*\*. Shared variables MUST be

read/written through the lock at every access, never through diff. Crossing this boundary

at runtime is the S2→S3 promotion point (Section 5.4), which installs the publish fence to

prevent speculative (unvalidated) values from being observed by another vt (JMM

out-of-thin-air prohibition).



\### 11.6. JMM Conformance (JSR 133)



| JMM construct | Requirement |

|---------------|-------------|

| `volatile` read/write | MUST compile to atomic `load(Acquire)` / `store(Release)`. `volatile` fields do NOT use the object lock (lock-free ordered). A `volatile` write MUST make all prior ordinary writes visible to a subsequent `volatile` read. |

| `synchronized` | monitorenter = Acquire, monitorexit = Release; \*\*reentrant\*\* (owner + count). Uses the reentrant monitor structure, NOT the u16 field lock (Section 12.1). |

| `final` fields | Freeze semantics: after construction completes, any thread obtaining a correctly-constructed reference MUST observe initialized `final` fields. Enforced by a freeze fence at the publication point (S2→S3 promotion, Section 5.4). |

| happens-before | Established transitively via the above plus program order within a vt. |

| out-of-thin-air | Speculative values MUST NOT be published to other vt before validation (enforced at promotion). |



\*\*ARM64 note:\*\* ARM has a weak memory model. `volatile` and publication fences MUST be

emitted explicitly with the correct `Acquire`/`Release`/`AcqRel` orderings; omission

produces bugs that manifest only on mobile hardware.



\---



\## 12. Locking \[NORMATIVE]



\### 12.1. Two Distinct Lock Facilities



\- \*\*Reentrant monitor\*\* (for `synchronized`): a structure `{ owner: AtomicVtId, count: u32,

&#x20; wait\_queue }`. `monitorenter`: if `owner == self`, `count += 1`; else acquire. Supports

&#x20; `wait/notify`. This is NOT the u16 field lock.

\- \*\*Field lock\*\* (for compound field transactions like `x += 1`): the compact u16/u32 lock

&#x20; below. Short critical sections, non-reentrant.



Mixing these is prohibited (STD constraint): do not use the compact lock for

`synchronized`.



\### 12.2. Compact u16 Read/Write Lock



```

bit 15..2 (14 bits): reader count  (max 16383 concurrent readers)

bit 1              : write-waiting  (blocks new readers; prevents writer starvation)

bit 0              : write-held

```



```rust

struct U16Lock(AtomicU16);

const W\_HELD: u16 = 0b01;

const W\_WAIT: u16 = 0b10;

const R\_UNIT: u16 = 0b100;



impl U16Lock {

&#x20;   fn read(\&self) {

&#x20;       loop {

&#x20;           let s = self.0.load(Acquire);

&#x20;           if s \& (W\_HELD | W\_WAIT) == 0 {

&#x20;               if self.0.compare\_exchange\_weak(s, s + R\_UNIT, Acquire, Relaxed).is\_ok() {

&#x20;                   return;

&#x20;               }

&#x20;           } else { park\_or\_spin(); }

&#x20;       }

&#x20;   }

&#x20;   fn read\_unlock(\&self) { self.0.fetch\_sub(R\_UNIT, Release); }



&#x20;   fn write(\&self) {

&#x20;       loop { // 1. claim W\_WAIT (block new readers)

&#x20;           let s = self.0.load(Acquire);

&#x20;           if s \& W\_WAIT == 0 {

&#x20;               if self.0.compare\_exchange\_weak(s, s | W\_WAIT, Acquire, Relaxed).is\_ok() { break; }

&#x20;           } else { park\_or\_spin(); }

&#x20;       }

&#x20;       loop { // 2. wait for readers to drain, claim W\_HELD, clear W\_WAIT

&#x20;           let s = self.0.load(Acquire);

&#x20;           if s >> 2 == 0 \&\& s \& W\_HELD == 0 {

&#x20;               if self.0.compare\_exchange\_weak(s, W\_HELD, AcqRel, Relaxed).is\_ok() { return; }

&#x20;           } else { park\_or\_spin(); }

&#x20;       }

&#x20;   }

&#x20;   fn write\_unlock(\&self) { self.0.store(0, Release); }

}

```



\- Read requires both `W\_HELD` and `W\_WAIT` clear (write-waiting blocks new readers to

&#x20; prevent writer starvation).

\- Write sets `W\_WAIT`, drains readers, then claims `W\_HELD` and clears `W\_WAIT`.



\### 12.3. park\_or\_spin (Poll-style degradation)



```rust

fn park\_or\_spin(/\* ctx \*/) {

&#x20;   for \_ in 0..SPIN\_LIMIT { std::hint::spin\_loop(); /\* re-check, return if state changed \*/ }

&#x20;   park\_on(self\_addr); // degrade to park; carrier reclaimed by rayon/tokio

}

```



Short critical sections spin (cheap on mobile multicore); long waits degrade to park,

yielding the carrier. Unlock paths MUST `notify` once.



\### 12.4. Multi-Lock Atomicity



A single statement requiring multiple locks MUST acquire them \*\*all-or-nothing\*\* (transaction

atomicity). Locks MUST be acquired in a global order (e.g., by object id) to prevent

deadlock; if not all can be acquired, all held locks MUST be released and the acquisition

retried (a transaction MUST NOT hold a partial set of its required locks). When two

statements each need a read and a write lock on the same object (mutually exclusive), one

MUST release and wait until the other commits.



\### 12.5. Fences



`volatile` bypasses the lock entirely (12.2 note; Section 11.6). The `final` freeze fence is

emitted at the publication point (Section 5.4).



\### 12.6. 128-bit Atomicity Constraints (ARM64)



Because reference values are 128-bit:



| Concern | Fact | Requirement |

|---------|------|-------------|

| CASP availability | CASP is ARMv8.1 LSE; many low-end Android devices are v8.0 (no CASP). | MUST use `portable-atomic` (feature-detected LDXP/STXP LL-SC fallback). MUST NOT rely on unstable `std` 128-bit atomics. |

| Alignment | 128-bit atomics require 16-byte alignment. | The allocator MUST enforce 16-byte alignment for atomically-accessed references. |

| iOS | A11+ has LSE. | Only Android long-tail needs the fallback. |

| Torn reads | Mutable metadata in a copied 128-bit value would tear. | Mutable validity/rc MUST live in the control block, not the copied pointer (Section 4.4). |



\### 12.7. Lock Sizing and Hot-Object Upgrade



\- S2 objects that require locking (rare) MAY use the u16 lock (14-bit reader count).

\- Hot S3 objects (e.g., Minecraft `World`, chunk/entity lists) with potentially thousands of

&#x20; concurrent readers MUST use a \*\*u32 lock\*\* (30-bit reader count) to avoid reader-count

&#x20; overflow. Upgrade to u32 happens at the S2→S3 promotion point, incurring 2 extra bytes only

&#x20; for hot objects; cold objects retain u16.



\---



\## 13. JIT Compilation \[NORMATIVE]



\### 13.1. Backends



The JIT MUST support at least one of, and SHOULD support both:



\- \*\*LLVM via `inkwell`\*\* — high optimization (O2/O3), slower compilation. Suitable for AOT

&#x20; and for the highest-value hot regions.

\- \*\*Cranelift\*\* — faster compilation, good baseline code. Preferred for low-latency online

&#x20; JIT.



Backend selection MAY be per-region, weighted by expected residency.



\### 13.2. Compilation Unit and Trigger



\- The JIT MUST match against the \*\*runtime L2 execution stream\*\* (the op-stream), producing

&#x20; IR via a builder, then compiling. Generated LLVM/Cranelift IR SHOULD be cached (`.ll`) to

&#x20; avoid regeneration on re-optimization.

\- Hotness MUST be measured by \*\*scope-level accumulated execution time\*\* (time share),

&#x20; identifying hot regions by top-k time occupancy — independent of call shape. This avoids

&#x20; mis-triggering on polymorphic-but-cheap code.

\- JIT compilation MUST be \*\*asynchronous\*\*. On completion, the compiled blob id is inserted

&#x20; into the dispatch table and the `accel` field is CAS'd with a single `notify`.

\- Compiled fragments are divided \*\*by scope\*\*.

\- Cache management SHOULD use an LRU weighted by performance gain per unit result size, with

&#x20; reinforced hotspot analysis.



\### 13.3. Fragment Replacement



Replacing an op's `accel` indicator with a compiled-blob id is the fragment-replacement

mechanism (a single-word CAS, Section 9.3). JNI call sites are handled identically:

`rjavavm` calls captured at L2 are replaced with pointers to native thunks (Section 16).



\### 13.4. No Deoptimization



RustyJVM MUST NOT require a deoptimization engine. The two root causes of deopt in HotSpot

are eliminated by design (P-3):



| HotSpot deopt cause | RustyJVM avoidance |

|---------------------|--------------------|

| Speculative devirtualization baked into native code | Virtual dispatch goes through a \*\*runtime mutable table\*\*; a new class adds a table entry; native code stays valid (zero deopt). |

| Speculative type/branch assumptions baked into native code | Speculation lives in the \*\*interpreter/diff layer\*\*, never in committed native code; mis-speculation discards unlanded diffs (oneshot, Section 10.3); no native-frame reconstruction. |

| Class redefinition / mixin invalidating hot code | Mixin occurs at \*\*class-load time\*\* (before the class is hot / instantiated); JIT is proactively invalidated → short interpretation → re-JIT. This is not a mid-execution event. |



\*\*On-Stack Replacement (OSR)\*\* in the \*interpreter → JIT\* direction is permitted and used

(async JIT completion inserts into the dispatch table + notifies once). OSR occurs only at

safepoints (loop back-edges) and is not deoptimization.



\### 13.5. Verification-Enabled Specialization



Using the static type proofs from L1 verification (Section 7.5), the JIT MUST specialize

values into native unboxed representations within regions proven type-safe: an `int` becomes

a native `i32` in a register, and that native code carries neither the 128-bit tag nor a tag

check. The tag machinery lives only in the interpreter's uniform representation; the JIT

evaporates the 128-bit tagged value plus tag mechanism together. This is where native

performance is realized, and it is achieved via \*\*static proof\*\*, not speculation

(consistent with no-deopt).



\### 13.6. Floating-Point \[NORMATIVE]



The JIT MUST NOT enable fast-math or any non-IEEE-754-strict optimization. Java 17+ is

strict-FP by default (JEP 306). IEEE 754 determinism is required for Minecraft world

generation (seed → terrain is float-deterministic) and for save-file compatibility with

Corretto. Non-strict math would diverge from the conformance base.



\### 13.7. Platform Gating



The JIT is \*\*conditionally compiled\*\*. On iOS it MUST be disabled at build time (Section

24.2). `rjava-std` MAY be JIT-compiled where the platform permits.



\---



\## 14. Ahead-of-Time Compilation \[NORMATIVE]



\### 14.1. Purpose and Model



AOT compiles bytecode to native static libraries at build/package time, primarily to satisfy

iOS's prohibition on runtime code generation, and as an optional startup/performance

optimization on all platforms.



\- Two workflows:

&#x20; 1. \*\*Profiled\*\*: pre-profile a JAR → compile to L1 representation → static hotness

&#x20;    analysis → compile hot regions to `.a` → package.

&#x20; 2. \*\*Runtime-recorded\*\* (non-iOS only): run unprocessed JAR → record hotness → generate

&#x20;    `.a` via the translator and cache; cache the L1 representation as `.rjvml1`.

\- Runtime generation of `.a` is \*\*impossible on iOS\*\* (code signing forbids executing

&#x20; runtime-generated machine code; dyld does not load unsigned/pre-unlinked native libs).

&#x20; iOS MUST use only build/package-time AOT.



\### 14.2. Constraints



\- Only classes provably not transformed at load time may be AOT'd (Section 8.7). Mixin

&#x20; targets MUST NOT be AOT'd.

\- The interpreter MUST remain a valid fallback for all non-AOT'd code (P-1). Three execution

&#x20; paths (interpreter + AOT native + JIT native) coexist; the interpreter is always correct.

\- On iOS, mixin'd mods run interpreter-only (physical lower bound).



\### 14.3. Artifacts



AOT output is a native static library (`.a`) linked into the host binary at package time,

plus the cached L1 (`.rjvml1`) and, where applicable, LLVM IR (`.ll`) and JIT results within

the `.rjvmc` cache (Section 9.3). On non-iOS, sibling `.ll` and `.o`/`.dll`/`.dylib` files

MAY be produced for reuse. This is a general-JDK facility; fragments are divided by scope.



\---



\## 15. Invokedynamic and MethodHandles \[NORMATIVE]



\### 15.1. Bootstrap Method Dispatch



`invokedynamic` bootstrap methods (BSMs) fall into two classes; both MUST be supported:



\- \*\*Standard BSMs\*\* — `java.lang.invoke.LambdaMetafactory`, `StringConcatFactory`, record

&#x20; `ObjectMethods` — MUST be implemented natively in Rust (`rjava-std`) to obtain native

&#x20; performance. BSMs may be polymorphic.

\- \*\*User/framework-defined BSMs\*\* — these are Java bytecode and MUST run on the

&#x20; interpreter/JIT. A fallback path treating "a BSM is an ordinary Java method" is REQUIRED.

&#x20; Because a `CallSite` is linked once and subsequent calls hit directly, BSM execution cost

&#x20; is amortized.



\### 15.2. MethodHandle Chain Folding



`MethodHandle` adapter chains (`asType`, `insert`, `bind`, LambdaForm layers) MUST be

folded/inlined by a dedicated small compiler. Per-layer interpretation of MH chains would be

catastrophically slow for stream-heavy and lambda-heavy code. HotSpot achieves speed via

LambdaForm compilation + forced inlining; RustyJVM MUST provide an equivalent

folding/inlining path, not merely a correct-but-slow interpreter.



\### 15.3. Runtime-Generated Classes



`LambdaMetafactory` generates hidden classes at runtime. These are \*\*bytecode\*\*, not machine

code, so interpretation is legal on iOS. Such wrapper classes CANNOT be AOT'd; however, the

lambda body (a private static method of the original class) CAN be AOT'd. Implementation

priority for standard BSMs: `StringConcatFactory` (high-frequency, tractable),

`LambdaMetafactory` (mandatory), record `ObjectMethods` (simple).



\---



\## 16. Native Bridge and Graphics \[NORMATIVE]



\### 16.1. JNI Model



JNI is implemented as dynamic-library loading with function-pointer extraction. Native

methods invoked by `rustystd` MUST be routed through special `rjavavm` calls; these calls

are captured at L2 and, like JIT results, replaced with pointers to native thunks (Section

13.3). This reuses the JIT indicator/replacement machinery.



\- On iOS, there MUST be no `dlopen` of unsigned native libraries. Native functionality

&#x20; required by the guest MUST be provided by statically linked Rust implementations

&#x20; (Section 16.3).



\### 16.2. Graphics: Unified wgpu Backend



The graphics layer MUST use \*\*`wgpu`\*\* as a unified backend, targeting:



\- Windows / Linux / Android → Vulkan

\- macOS / iOS → Metal



This replaces OpenGL across all platforms.



\### 16.3. LWJGL Redirection



Minecraft calls the \*\*LWJGL Java API\*\* for windowing, input, audio, and graphics. RustyJVM

MUST provide a redirection layer that maps these Java native calls to the `wgpu`/Rust

implementation, rather than dlopen'ing system GL. This layer determines whether Minecraft

renders. On iOS it MUST NOT perform real JNI dlopen of unsigned libraries; the LWJGL surface

is backed by statically linked Rust.



\### 16.4. Security of Native Endpoints



Any network-exposed or IPC endpoint introduced by the native layer MUST document its

authentication/authorization posture. The native bridge MUST NOT silently create

unauthenticated network services.



\---



\## 17. Bootstrap Sequence \[NORMATIVE]



\### 17.1. The Genesis Problem



Core class identity is circular: `Class` objects are themselves objects; `java.lang.Class`

is an instance of `java.lang.Class`; `ClassLoader` must load classes but must itself be

loaded. The interpreter MUST hand-fabricate the first core objects (analogous to HotSpot's

`Universe::genesis()`).



\### 17.2. Fabrication Order and Invariants



1\. Allocate untyped raw mirrors for `Object`, `Class`, `String` (and other minimal core

&#x20;  klasses).

2\. Back-fill type pointers and hierarchy:

&#x20;  - `Object.super = null` (MUST — `Object.class.getSuperclass()` returns `null` in a

&#x20;    conformant JVM; setting it to itself would fail differential tests).

&#x20;  - `Class.super = Object`.

&#x20;  - `Class.klass = Class` (self-reference — `Class.class.getClass() == Class.class`; the

&#x20;    self-reference is on the \*\*type/klass pointer\*\*, not the superclass chain).

&#x20;  - `Object.klass = Class`.

3\. The bootstrap loader is represented as `null` at the Java level (`getClassLoader()`

&#x20;  returns `null` for bootstrap classes).

4\. After genesis, normal loading proceeds on the established foundation.



The interpreter is the source of truth for these fabricated definitions (P-1).



\---



\## 18. Standard Library (rustystd) \[NORMATIVE]



\### 18.1. Scope



`rustystd` reimplements the Java standard library (`java.base` and required modules) in

Rust, including native method implementations. It is a pure executor, has no reverse

dependency on higher crates, and depends on `rjava-core` for shared struct definitions and

on `rjava-interp` only for reentry.



\### 18.2. Behavioral Conformance



`rustystd` MUST match the \*\*observable behavior\*\* of the Corretto conformance base,

\*\*including bug-for-bug behavior\*\* that guests depend on. "Fixing" an upstream quirk that

Minecraft or a library relies on is a conformance violation (P-5).



\### 18.3. Coverage Requirements



At minimum, `rustystd` MUST implement, with conformant semantics: `java.lang.\*` (including

`Object`, `String`, `Class`, `Thread`, `System`, wrappers, `Throwable` hierarchy),

`java.util.\*` (collections with correct iteration/fail-fast semantics), `java.util.concurrent.\*`,

`java.io.\*`, `java.nio.\*`, `java.net.\*`, `java.lang.reflect.\*`, `java.lang.invoke.\*`

(Section 15), `java.lang.ref.\*` (Section 6.6), and the `Unsafe`/`VarHandle` surface

(Section 19.3).



\### 18.4. Additional Semantics



\- \*\*Reflection + `MethodHandles.Lookup`\*\*: complete class metadata MUST be available; a

&#x20; framework/mod hard requirement.

\- \*\*Array covariance\*\*: `ArrayStoreException` MUST be raised per JLS on covariant array

&#x20; stores.

\- \*\*String interning\*\*: identity semantics of the interned string pool MUST be conformant.

\- \*\*Serialization\*\*: serialized forms MUST be Corretto-compatible.



\---



\## 19. Version Strategy and Corretto Conformance \[NORMATIVE]



\### 19.1. Supported Versions and Support Tiers



RustyJVM MUST support the Java versions required by target workloads and typical Java

applications:



| Version | Classfile | Support tier | Notes |

|---------|-----------|--------------|-------|

| Java 21 | v65 | \*\*Mid-term support\*\* (primary) | Virtual threads (Loom) GA — aligns with the vt model; generational ZGC; pattern matching/records. Minecraft 1.20.5+. |

| Java 17 | v61 | Build-then-freeze | JPMS mature; strictfp default. Minecraft 1.18–1.20. |

| Java 8  | v52 | Build-then-freeze | No module system; monolithic rt.jar; `sun.misc.Unsafe` unrestricted. Minecraft ≤ 1.16 + legacy mods. |

| Java 26 | v70 | \*\*Future primary\*\* | `sun.misc.Unsafe` largely removed; stricter encapsulation; possible Valhalla value types. Deferred until stable. |



\*\*Implementation order (MANDATORY):\*\* 21 → 17 → 8 → 26. Rationale: 21 aligns best with the

vt/modern semantics; 8 is largely a subset of 17 minus modules (degradation is easier to add

than modules); 26 is deferred until Valhalla stabilizes to avoid rework.



\### 19.2. Single Library, Version Gate



`rustystd` MUST be a single implementation gated by a runtime `ClassfileVersion` into two

capability tiers:



\- \*\*Legacy tier (8)\*\*: no module system, full `Unsafe`, non-strict FP, monolithic library

&#x20; view.

\- \*\*Modern tier (17/21/26)\*\*: JPMS module visibility, restricted `Unsafe` → `VarHandle`,

&#x20; strict FP, virtual threads.



Forking the codebase per version is prohibited (P-6).



\### 19.3. Unsafe / VarHandle Drift



`sun.misc.Unsafe` availability differs by version: full (8), restricted (17), largely

removed (26). Because `Unsafe` is a hard requirement of Minecraft, legacy mods, and many

libraries (off-heap, field offsets, CAS), `rustystd` MUST present a version-appropriate

`Unsafe` surface: full for 8; for 26, only the `java.lang.invoke.VarHandle` replacements.



\### 19.4. Module System



Java 9+ JPMS changes classloader topology (platform loader, module graph, `requires/exports`

visibility). The Modern tier MUST enforce module readability/accessibility in the

ClassRegistry; the Legacy tier (8) MUST NOT. `SecurityManager` is NOT implemented (deprecated

JEP 411/17, disabled JEP 486/24); avoiding permission complexity is consistent with modern

Java direction.



\### 19.5. Corretto Conformance Contract



RustyJVM's \*\*observable behavior\*\* — class-library semantics, exception types and timing,

reflective results, string/numeric/IEEE-754 float semantics, serialized forms — MUST match

\*\*Corretto 21\*\* (and Corretto 17 / 8 for those tiers). \*\*Unobservable\*\* implementation

details — GC, JIT internals, memory layout, thread scheduling — are free. Verification is by

\*\*differential testing\*\*: for the same input, RustyJVM output MUST equal Corretto output.

The specification base (JVMS/JLS/JMM) is upstream OpenJDK, which Corretto does not modify.



\### 19.6. Floating-Point Determinism



Per Section 13.6: strict IEEE 754, no fast-math. Differential FP tests MUST use the

version-appropriate oracle (Java 8 non-strict-default behavior differs from 17+ strict

default; testing 8 code against a 21 oracle would produce false positives).



\---



\## 20. Exception Handling \[NORMATIVE]



\### 20.1. Implicit Throw Points



Nearly every instruction can throw implicitly: `getfield`/`putfield`/`invoke` → `NPE`;

array access → `ArrayIndexOutOfBoundsException`; `idiv` → `ArithmeticException`; `checkcast`

→ `ClassCastException`; any allocation → `OutOfMemoryError`; any call → `StackOverflowError`.



\### 20.2. Precise Exception Semantics



JVMS requires precise exceptions: all side effects before the throwing instruction MUST be

visible; none after. RustyJVM MUST guarantee this.



\### 20.3. Control Edges — Minimal Set



\- L1 MUST install an \*\*implicit throw control edge\*\* only for instructions that \*\*can throw

&#x20; AND are covered by a handler in the current method\*\* (`Effect::MayThrow { caught: true }`),

&#x20; using the exception table.

\- Instructions that can throw but are \*\*not\*\* locally covered require no control edge: an

&#x20; uncaught throw unwinds the frame, and the entire frame's diffs are destroyed with the

&#x20; frame. Thus non-`try` instructions issue freely.



\### 20.4. Uncaught Throw



When program order reaches a throwing instruction whose exception is not caught in the

current method, the scope is exited and its remaining (unlanded) diff tree is destroyed

naturally. Because landing is po-incremental (Section 10.5), all diffs with `po < throw`

have already landed; only `po > throw` diffs are discarded.



\### 20.5. Locally-Caught Throw — Program-Order Truncation



For an exception caught within the current method, the throw point is a \*\*program-order

truncation point\*\*, NOT a whole-scope destruction:



\- Diffs with `po < throw` (e.g., ordinary local writes preceding the throw) MUST have landed

&#x20; and MUST be visible to the handler.

\- Diffs with `po > throw` on the abandoned path MUST be discarded.

\- Control transfers to the handler.



Example: `int x=0; try { x=5; risky(); x=10; } catch(E e){ print(x); }` — the handler MUST

observe `x == 5` (the `x=5` write is an intrinsic local write that is not fenced; whole-scope

destruction would incorrectly lose it). po-incremental landing makes this automatic.



\### 20.6. Exception Table and finally



`try`/`catch`/`finally` and overlapping handler regions MUST be honored per the class file's

exception table. `finally` blocks and their interaction with abnormal completion MUST match

Corretto behavior.



\### 20.7. Stack Traces



`fillInStackTrace()` MUST read the \*\*logical Java call chain\*\*, which differs from execution

order under out-of-order/inlined execution. The logical call chain MUST be maintained

\*\*independently of execution order\*\* (inlined sub-scopes MUST record which logical Java frame

they belong to). Minecraft/Forge parse stack traces extensively; incorrect logical frames

produce silent, hard-to-diagnose bugs.



\---



\## 21. Technology Stack \[NORMATIVE]



| Concern | Choice | Rationale |

|---------|--------|-----------|

| Language | Rust (edition 2021+) | Memory safety for the host; ownership model enables S1 zero-cost. |

| 128-bit atomics | `portable-atomic` | Feature-detected CASP/LDXP-STXP; avoids unstable std 128-bit atomics; covers Android v8.0 long tail. |

| Data parallelism / workers | `rayon` | Worker dispatch for runnable diff chains (Section 10.2). |

| Async carriers | `tokio` (optional) | Carrier for parked/blocking work; NOT 1:1 Java threads. |

| JIT backend (high-opt) | LLVM via `inkwell` | O2/O3, AOT and highest-value regions. |

| JIT backend (low-latency) | `cranelift` | Fast online JIT; often better latency than LLVM for baseline. |

| Graphics | `wgpu` | Unified Vulkan/Metal backend. |

| Class file / serialization format | FlatBuffers (for `.rjvml1`/`.rjvmc`) | Zero-copy, schema-validated, fast load. Format validity MUST be checked on load. |

| Concurrent maps | `dashmap` or a custom lock-free skip list | ClassRegistry and lookup tables (Section 10.1). |

| Hashing (cache keys) | Non-cryptographic for CRC-style integrity; HMAC/signature if tamper-resistance is required | CRC32 detects corruption, not tampering (Section 7.1). |



Dependencies MUST be pinned to exact versions. Unusual or typosquat-suspect package names

MUST be flagged.



\*\*FlatBuffers note:\*\* RustyJVM as a library trusts the caller to provide valid data; it also

validates FlatBuffer structural format on load. However, if the op-stream content is

tampered with AND the CRC32 is correspondingly updated, integrity checking cannot detect it

(CRC32 is non-cryptographic). Tamper-resistant deployments MUST use HMAC/signatures. This

does not affect host memory safety (Section 4.3), only content authenticity.



\---



\## 22. Cross-Cutting Requirements \[NORMATIVE]



\### 22.1. Error Handling and Diagnostics



\- All guest-visible error conditions MUST map to the JVMS-specified exception/error type and

&#x20; timing (STD-CODE-4).

\- Internal invariant violations that cannot occur for verified code (e.g., registry index

&#x20; out of bounds) MUST be treated as defense-in-depth aborts with diagnostic context, never

&#x20; silently ignored.

\- The allocation-failure protocol: a failed allocation places an error enum into the slot

&#x20; reserved for the handle; the handle/object are not spontaneously freed (the handle is held

&#x20; by the allocating process). Reaching the failing allocation statement retrieves the error

&#x20; and raises it. If handled, subsequent statements proceed; if unhandled, the handle (and

&#x20; object) are destroyed on scope exit. Allocation statements MUST NOT have prerequisite

&#x20; dependencies (else the object cannot be pre-allocated, since fields cannot be predicted).



\### 22.2. Security Considerations



\- Host memory safety holds for adversarial bytecode (Section 4.3).

\- Verification is the guest-type-safety floor (Section 7).

\- Treat all class file, command output, and external content as untrusted data; embedded

&#x20; "instructions" in guest data MUST NOT alter engine behavior.

\- Files likely to contain secrets MUST NOT have their secret values echoed in logs.

\- The native/graphics layer MUST NOT create unauthenticated network endpoints silently

&#x20; (Section 16.4).



\### 22.3. Observability



The runtime SHOULD expose counters for: mis-speculation rate per region (required for

Section 10.7 gating), JIT compilation latency and cache hit rate, GC cycle-collection

frequency and pause durations, and per-tier object population (S1/S2/S3).



\---



\## 23. Testing and Conformance \[NORMATIVE]



\### 23.1. Differential Testing



Every normative behavior MUST have a differential test against the Corretto oracle for the

active version (STD-CODE-3). Each support tier MUST have its own oracle: Corretto 8 / 17 /

21; Java 26 uses an upstream EA build until Corretto 26 GA.



\### 23.2. Conformance Suites



The implementation SHOULD be exercised against the OpenJDK `jtreg` regression suite and,

where licensing permits, the JCK/TCK. Corretto passes the TCK and serves as the practical

oracle for differential testing.



\### 23.3. Property and Fuzz Testing



\- The verifier MUST be fuzzed with malformed and adversarial class files; no input may cause

&#x20; host memory unsafety.

\- The concurrency/JMM layer MUST be tested under stress on weak-memory (ARM64) hardware,

&#x20; including targeted tests for `volatile` visibility, `final` freeze, and

&#x20; publication-fence correctness.

\- The GC MUST be tested for cycle reclamation (strong cycles must not leak) and for

&#x20; fork-snapshot root correctness (objects in active speculation must not be reclaimed).



\### 23.4. Floating-Point Determinism Tests



World-generation-style float pipelines MUST produce bit-identical results to the

version-appropriate oracle (Section 19.6). Save-file compatibility is a conformance gate for

the Minecraft workload.



\### 23.5. Milestone Definition of Done



\- \*\*M1 (Java 21 baseline):\*\* interpreter runs verified class files; `rustystd` core;

&#x20; passes a defined differential subset; runs Minecraft 1.20.5+ on interpreter.

\- \*\*M2:\*\* JIT + AOT; wgpu/LWJGL redirection; playable Minecraft on desktop and Android.

\- \*\*M3:\*\* iOS AOT path; interpreter-only mixin mods on iOS.

\- \*\*M4:\*\* Java 17, then Java 8 (Legacy tier); broadened conformance.

\- \*\*M5:\*\* Java 26 (future primary).



\---



\## 24. Platform-Specific Requirements \[NORMATIVE]



\### 24.1. Android



\- Target Bionic + AArch64. Handle Bionic C-library differences (e.g., missing

&#x20; `ucontext.h`/functions) relative to glibc.

\- JIT permitted. 128-bit atomics MUST use `portable-atomic` for v8.0 devices (Section 12.6).



\### 24.2. iOS



\- \*\*JIT MUST be disabled at build time\*\* (code signing forbids runtime code generation;

&#x20; dyld does not load unsigned/pre-unlinked native libs). Runtime `.a` generation is

&#x20; impossible.

\- AOT is the only native-acceleration path; only build/package-time AOT is valid.

\- No `dlopen` of unsigned native libraries; native functionality via statically linked Rust.

\- Mixin'd/runtime-generated classes run interpreter-only.

\- Metal via `wgpu`; A11+ has LSE (no atomic fallback needed).



\### 24.3. Desktop (Windows/Linux/macOS)



\- Full JIT + AOT. Vulkan (Win/Linux) / Metal (macOS) via `wgpu`.

\- Without GC pauses on the S1 fast path, interpreter+JIT performance is expected to be

&#x20; competitive even on desktop.



\---



\## 25. Rejected Alternatives Summary \[RATIONALE]



Recorded to prevent re-litigation:



1\. \*\*Compile OpenJDK's C++ class library and bridge via JNI\*\* — inherits the C++ "mountain"

&#x20;  and ABI compatibility burden; contrary to the clean-room, single-core goal.

2\. \*\*Pure "no GC" via RAII/reference counting alone\*\* — leaks strong cycles; unsound for

&#x20;  Java (Section 6.1).

3\. \*\*Brownbridge strong/weak auto-demotion\*\* — unsound (dangling references); repair cost

&#x20;  exceeds tracing (Section 6.1).

4\. \*\*Per-node outer-world reachability cache with invalidate-on-rc-change\*\* — livelocks under

&#x20;  Minecraft mutation rates; direction error (Section 6.1).

5\. \*\*Statement-granularity speculation\*\* — worst-case tracking overhead; loses to serial

&#x20;  JIT on pointer-chasing/polymorphic workloads. Replaced by longest-dependency-chain,

&#x20;  load-gated speculation (Section 10.3, 10.7).

6\. \*\*Deoptimization engine\*\* — unnecessary given runtime mutable dispatch + interpreter-layer

&#x20;  speculation (Section 13.4).

7\. \*\*tokio as 1:1 Java thread mapping\*\* — cooperative async conflicts with blocking Java

&#x20;  semantics; use vt on carriers (Section 11.1).

8\. \*\*64-bit tagged values\*\* — insufficient payload after tag for reference index + metadata

&#x20;  (Section 4.1).

9\. \*\*Mutable validity/rc inside the copied 128-bit pointer\*\* — 128-bit atomic + torn-read

&#x20;  hazard on every copy; use a separate control block (Section 4.4, 12.6).

10\. \*\*CRC32 as a substitute for bytecode verification\*\* — orthogonal concerns; CRC32 cannot

&#x20;   detect malicious well-formed bytecode (Section 7.1).

11\. \*\*Static ordering of `<clinit>`\*\* — impossible for dynamically loaded classes; use

&#x20;   runtime `ensure\_init` guards (Section 8.5).



\---



\## 26. Glossary of Artifacts and File Types



| Artifact | Meaning |

|----------|---------|

| `.rjvml1` | Verified, flattened L1 SSA representation (trusted after production). Cache key includes `(loader, name)` and post-transform byte hash. |

| `.rjvmc` | On-disk compiled cache = `.rjvml1` + JIT results. (Renamed from the ambiguous "l2".) |

| `.ll` | Cached LLVM IR for a fragment (reuse across re-optimization). |

| `.a` / `.o` / `.dll` / `.dylib` | AOT/JIT native outputs (platform-dependent; `.a` for iOS static linking). |

| op-stream-feed | The runtime L2 instruction stream (not persisted under this name). |



\---



\## 27. Document Status and Change Control



This document is DRAFT pending secondary agent review. Changes to \*\*\[NORMATIVE]\*\* sections

MUST be versioned per STD-VER-1. \*\*\[RATIONALE]\*\* rejections MUST NOT be reversed without new

evidence recorded in Section 25. Multi-file decomposition of this specification is performed

downstream by the maintainer and is out of scope for this document.



\--- END OF RJVM-SPEC-001 ---

