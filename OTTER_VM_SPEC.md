# Otter VM Specification

## 1. Scope

This document specifies the Otter VM architecture, performance strategy, and implementation roadmap. It serves as the authoritative reference for VM internals.

### 1.1 VM Scope (This Document)

The VM layer is responsible for:

- **Semantics**: ECMAScript specification compliance, bytecode execution
- **Performance primitives**: Inline caches, type profiling, JIT compilation
- **Memory model**: Garbage collection, value representation, object layout
- **Diagnostics**: Source maps, error stack traces, debugging hooks
- **Core builtins**: Object, Array, String, Number, Boolean, Symbol, BigInt, Function, Error, RegExp, Promise, Date, Math, JSON, Map, Set, WeakMap, WeakSet, Proxy, Reflect

### 1.2 Runtime Scope (Required, Feature-Gated)

The runtime layer provides JavaScript execution environment features. This is **required** for Otter to be useful but is **feature-gated** for embedded use cases that provide their own event loop.

- **Event loop**: Task scheduling, I/O polling, timer management
- **Timers**: setTimeout, setInterval, setImmediate, clearTimeout, clearInterval
- **Microtasks**: Promise.then queue, queueMicrotask
- **I/O APIs**: fetch, fs, net, http, WebSocket (capability-gated)
- **Workers**: Worker threads with message passing (future)

### 1.3 Tooling Scope (Separate Document, Non-Blocking)

Tooling does **not** block VM development and is documented separately:

- **Bundler**: Tree-shaking, code splitting, output optimization
- **Package manager**: Dependency resolution, lockfiles, registry interaction
- **Test runner**: Test discovery, execution, reporting

### 1.4 Non-Goals

- **Browser compatibility**: No DOM, BOM, or browser-specific APIs
- **Node.js full API parity**: Selective compatibility, not drop-in replacement
- **Competing on peak throughput**: Focus on embedded use cases, not long-running server workloads
- **Supporting all JS edge cases**: Prioritize common patterns over exotic spec corners

---

## 2. Current Architecture (As-Is)

### 2.1 Value Representation (NaN-Boxing)

Location: `crates/otter-vm-core/src/value.rs`

All JavaScript values are encoded in 64 bits using NaN-boxing:

| Type | Encoding |
|------|----------|
| `f64` (non-NaN) | IEEE 754 double |
| `undefined` | Quiet NaN + tag 0x1 |
| `null` | Quiet NaN + tag 0x2 |
| `boolean` | Quiet NaN + tag 0x3 + payload |
| `i32` | Quiet NaN + tag 0x4 + 32-bit payload |
| `pointer` | Quiet NaN + tag 0x5-0xF + 48-bit pointer |

**Strengths**: No heap allocation for primitives, efficient arithmetic, cache-friendly.

### 2.2 Heap Model (Current: Arc-Based)

Location: `crates/otter-vm-core/src/object.rs`, `crates/otter-vm-core/src/value.rs`

**Current state**: All heap objects use `Arc<T>` for reference counting.

**Problem**: Circular references leak memory. Common JS patterns (closures capturing parent scope, DOM-like trees) create cycles that Arc cannot collect.

**Existing GC crate**: `crates/otter-vm-gc/` contains mark-sweep and concurrent collector implementations but is **not integrated** with the runtime.

### 2.3 Object Model (Shapes + Inline Properties)

Location: `crates/otter-vm-core/src/shape.rs`, `crates/otter-vm-core/src/object.rs`

Objects use V8-style hidden classes (Shapes):

- **Shape**: Describes property layout (names, offsets, attributes)
- **Inline properties**: First 4 properties stored inline (no separate allocation)
- **Overflow properties**: HashMap for properties beyond inline slots
- **Prototype chain**: Shapes store prototype reference for inheritance

**Shape transitions**: Adding a property creates a new Shape, linked from the old one. This enables IC optimization.

### 2.4 Bytecode & Interpreter

Location: `crates/otter-vm-bytecode/src/`, `crates/otter-vm-core/src/interpreter.rs`

- **Register-based VM**: 256 registers per frame, no operand stack
- **70+ opcodes**: Arithmetic, property access, calls, control flow, closures
- **Variable-length encoding**: 1-byte opcode + variable operands
- **IC slots**: Instructions have `ic_index` fields for inline cache attachment

**Interpreter loop**: Switch-based dispatch with interrupt checks every N instructions.

### 2.5 Compiler Pipeline

Location: `crates/otter-vm-compiler/`

```
Source (JS/TS) → oxc parser → AST → Codegen → Bytecode
                                      ↓
                              Peephole optimizer
```

**oxc**: Industry-standard Rust parser with full ES2024+ support.

**Peephole optimizations** (current):

- Dead code elimination
- Copy propagation
- Register coalescing

---

## 3. Performance Pillars

Performance improvements are ordered by dependency: memory model must be stable before IC can be trusted, IC must work before type profiling is useful, etc.

### 3.1 Memory Model (GC + Rooting)

**Prerequisite for**: Everything else. IC entries point to shapes; shapes must not be collected while referenced.

#### 3.1.1 Rooting API

Before integrating GC, we need a safe rooting protocol:

```rust
// Proposed API
pub struct Gc<T>(NonNull<GcBox<T>>);  // Unrooted, only valid during single operation
pub struct Handle<T>(*mut GcBox<T>);   // Rooted, survives GC
pub struct HandleScope { ... }          // RAII scope for handles

impl HandleScope {
    pub fn new(ctx: &mut VmContext) -> Self;
    pub fn root<T>(&self, gc: Gc<T>) -> Handle<T>;
}
```

**Key invariant**: Code holding `Gc<T>` must not call any function that can trigger GC. Use `Handle<T>` across potential GC points.

#### 3.1.2 GC Strategy (Phased)

| Phase | Strategy | Notes |
|-------|----------|-------|
| Phase 0 | Rooting API + Arc→Gc migration | No collection yet, just rooting discipline |
| Phase 1 | Stop-the-world mark/sweep | Simple, correct baseline |
| Phase 2 | Incremental marking | Write barriers, reduced pause times |
| Phase 3 | Generational (optional) | Only if pause times are problematic |

**Phase 0-1 target**: Correct collection of circular references, bounded heap growth.

**Not in scope for Phase 1**: Concurrent marking, parallel scavenge, sub-millisecond pauses. These are optimizations for later.

### 3.2 Object Model (IC + Shapes + Proto Chain)

**Prerequisite for**: Type profiling, JIT. IC provides the fast path that makes optimization worthwhile.

#### 3.2.1 Inline Cache State Machine

```
Uninitialized → Monomorphic → Polymorphic (≤4) → Megamorphic
     ↑              ↓              ↓                  ↓
     └──────────────┴──────────────┴──────────────────┘
                    (Shape mismatch or invalidation)
```

**Monomorphic**: Single observed shape. Fast path: shape ID comparison + offset load.

**Polymorphic**: 2-4 observed shapes. Fast path: linear search through entries.

**Megamorphic**: Too many shapes. Fall back to hash lookup (no caching).

#### 3.2.2 IC Prerequisites (Currently Missing)

Before IC is useful, we need:

1. **Proto chain caching**: Cache not just the shape, but the prototype chain structure. Guard: shape ID + proto epoch.

2. **Dictionary mode transition**: When an object has too many properties or certain operations (delete, defineProperty with unusual descriptors), transition to dictionary mode. IC must detect this.

3. **Key caching**:
   - String interning (atom table) for property names
   - Array index fast path (numeric strings → integer indices)

4. **Epoch invalidation**: Operations that invalidate cached proto chain:
   - `Object.defineProperty` on prototype
   - `__proto__` assignment
   - `Object.setPrototypeOf`
   - `Object.preventExtensions` / `Object.seal` / `Object.freeze`

#### 3.2.3 IC Integration Points

| Opcode | IC Behavior |
|--------|-------------|
| `GetProp` / `GetPropConst` | Cache shape → offset |
| `SetProp` / `SetPropConst` | Cache shape → offset, may trigger transition |
| `CallMethod` | Cache shape → method value |
| `GetElem` | Cache if key is atom (interned string) |
| `GetGlobal` | Cache global object shape → slot |

### 3.3 Execution (Dispatch, Calls, Quickening)

#### 3.3.1 Interpreter Acceleration (Before JIT)

Before building a JIT, we can get significant gains from interpreter improvements:

**Superinstructions**: Fuse common opcode sequences:

- `GetPropConst` + `Call` → `CallPropConst`
- `GetLocal` + `Add` + `SetLocal` → `AddLocal`

**Threaded dispatch** (optional): Replace switch with computed goto for ~20% dispatch improvement. Requires careful assembly or compiler-specific extensions.

#### 3.3.2 Quickened Opcodes

Replace generic opcodes with type-specialized versions based on runtime feedback:

```
Add → AddI32  (if both operands always i32)
    → AddF64  (if both operands always f64)
    → AddAny  (fallback if mixed types)
```

**Quickening happens in interpreter**, not JIT. Each specialized opcode has a guard that falls back to generic version on type violation.

**Semantics guardrails**: Quickening is only safe when:

- Both operands have been observed as the target type
- The operation can fall back to generic on type mismatch
- No observable side effects from type coercion are skipped

**NOT safe to quicken**:

- `x * 2 → x << 1` (different semantics: ToNumber vs ToInt32, BigInt, valueOf)
- `x / 2 → x >> 1` (not equivalent for negative numbers)
- Any optimization that skips valueOf/toString calls

#### 3.3.3 Type Profiling

**Phase 1 (interpreter only)**: Lightweight per-opcode type bitmask.

```rust
struct TypeFeedback {
    seen_types: u8,  // Bitmask: i32, f64, string, object, etc.
}
```

**Phase 2 (with quickening)**: Use feedback to select quickened opcodes.

**Phase 3 (with JIT)**: Use feedback to generate specialized code with deopt guards.

### 3.4 Diagnostics (Source Maps, Errors)

#### 3.4.1 Source Location Mapping

Location: `crates/otter-vm-bytecode/src/function.rs`

Each bytecode instruction stores source location:

- File index (into module's file table)
- Line number
- Column number

**Stack traces**: Walk call stack, map bytecode offsets to source locations.

#### 3.4.2 Error Enhancement

Location: `crates/otter-vm-core/src/intrinsics_impl/error.rs`

- Parse source maps (`.map` files) when available
- Include original TypeScript locations in `Error.stack`
- Syntax errors include source snippet with caret

---

## 4. Roadmap

### Phase 0: Prerequisites (2-4 weeks)

**Goal**: Establish safe memory management foundation.

| Task | Files | Verification |
|------|-------|--------------|
| Design rooting API (`Gc<T>`, `Handle<T>`, `HandleScope`) | `otter-vm-core/src/gc.rs` | API review |
| Migrate `JsObject` from `Arc` to `Gc` | `otter-vm-core/src/object.rs` | Existing tests pass |
| Migrate `JsString` from `Arc` to `Gc` | `otter-vm-core/src/value.rs` | Existing tests pass |
| Implement STW mark/sweep | `otter-vm-gc/src/collector.rs` | Circular ref test |
| Connect GC to interpreter safepoints | `otter-vm-core/src/interpreter.rs` | No leaks under stress |

**Success metric**: Circular references are collected. Heap stays bounded under allocation stress.

### Phase 1: Correctness (4-8 weeks)

**Goal**: Complete missing language features, basic IC.

#### 1.1 Generators

| Task | Files | Verification |
|------|-------|--------------|
| Full frame snapshot (pc, sp, registers, catch_stack, env) | `otter-vm-core/src/generator.rs` | Test262 generators |
| `generator.next(value)` - send value into generator | `otter-vm-core/src/intrinsics_impl/generator.rs` | Protocol tests |
| `generator.return(value)` - force completion | `otter-vm-core/src/intrinsics_impl/generator.rs` | Finally semantics |
| `generator.throw(error)` - throw into generator | `otter-vm-core/src/intrinsics_impl/generator.rs` | Catch semantics |
| Async generators (`async function*`) | `otter-vm-core/src/generator.rs` | Test262 async-gen |

**Generator frame snapshot format**:

```rust
struct GeneratorState {
    pc: usize,                    // Program counter
    sp: usize,                    // Stack pointer
    registers: Vec<Value>,        // Register file snapshot
    catch_stack: Vec<CatchEntry>, // Exception handlers
    env_ptr: Handle<Environment>, // Lexical environment
    this_value: Value,            // Bound this
    new_target: Option<Value>,    // new.target if constructor
    pending_exception: Option<Value>, // For throw() protocol
}
```

#### 1.2 TypedArrays / ArrayBuffer

| Task | Files | Verification |
|------|-------|--------------|
| `ArrayBuffer` (wraps `Vec<u8>`, detach state) | `otter-vm-core/src/array_buffer.rs` | Test262 ArrayBuffer |
| TypedArray views (no-copy view into buffer) | `otter-vm-core/src/typed_array.rs` | Test262 TypedArray |
| `DataView` (arbitrary byte-order access) | `otter-vm-core/src/data_view.rs` | Test262 DataView |

**TypedArray view model** (critical: no copies):

```rust
struct TypedArrayView {
    buffer: Handle<ArrayBuffer>,  // Underlying buffer (NOT owned copy)
    byte_offset: usize,           // Offset into buffer
    length: usize,                // Number of elements
    element_size: usize,          // Bytes per element (1/2/4/8)
    kind: TypedArrayKind,         // Int8, Uint16, Float64, etc.
}
```

**Detach semantics**: When `ArrayBuffer.transfer()` or `ArrayBuffer.prototype.detach()` is called:

1. All TypedArray views become detached
2. Any access to detached view throws `TypeError`
3. `byteLength` returns 0

**SharedArrayBuffer**: Deferred until Atomics implementation. Must be designed together.

#### 1.3 Basic IC (Top 5 Opcodes)

| Task | Files | Verification |
|------|-------|--------------|
| IC for `GetPropConst` | `otter-vm-core/src/interpreter.rs` | Microbenchmark |
| IC for `SetPropConst` | `otter-vm-core/src/interpreter.rs` | Microbenchmark |
| IC for `GetGlobal` | `otter-vm-core/src/interpreter.rs` | Microbenchmark |
| IC for `CallMethod` | `otter-vm-core/src/interpreter.rs` | Microbenchmark |
| String interning (atom table) | `otter-vm-core/src/string.rs` | Property access tests |

**Success metric**: Monomorphic property access shows measurable improvement in microbenchmarks.

### Phase 2: Performance Infrastructure (6-10 weeks)

**Goal**: IC everywhere, quickening, type feedback.

#### 2.1 Complete IC Coverage

- IC for all property access opcodes
- IC state transitions (mono → poly → mega)
- Proto chain caching with epoch guards
- Dictionary mode detection

#### 2.2 Quickened Opcodes

- `AddI32`, `SubI32`, `MulI32` for integer fast paths
- `AddF64`, `SubF64`, `MulF64` for float fast paths
- Type guards with fallback to generic opcodes

#### 2.3 Type Feedback Collection

- Per-opcode type bitmasks
- Hot function detection (call count threshold)
- Feedback-driven quickening

**Success metric**: Numeric loops show 2-3x improvement. Property-heavy code shows 5-10x improvement.

### Phase 3: Baseline JIT (3-6 months)

**Goal**: Compile hot functions to native code.

#### 3.1 JIT Prerequisites (Before Writing Codegen)

| Prerequisite | Decision/Design |
|--------------|-----------------|
| GC integration | Non-moving heap + conservative stack scan (Phase 1) |
| Value representation | NaN-boxing in registers, same as interpreter |
| Calling convention | JIT code calls interpreter helpers for complex ops |
| Safepoints | At loop backedges and call sites |
| W^X policy | Separate RX and RW pages, no self-modifying code |

#### 3.2 JIT v1 Scope (Explicitly Limited)

**In scope**:

- Function-entry compilation (no OSR)
- Direct bytecode-to-Cranelift-IR translation
- Type guards based on collected feedback
- Simple bailout: on guard failure, rerun entire function in interpreter

**NOT in scope for v1**:

- On-Stack Replacement (OSR) - requires complex frame reconstruction
- Deopt snapshots - requires tracking all live values
- Inlining - requires interprocedural analysis
- Register allocation optimization - use Cranelift defaults

#### 3.3 JIT Architecture

```
Hot function detected (call_count > threshold)
        ↓
Collect type feedback from interpreter
        ↓
Generate Cranelift IR with type guards
        ↓
Cranelift compile to native code
        ↓
Patch function entry to jump to JIT code
        ↓
On guard failure: call interpreter fallback
```

**Success metric**: Test262 pass rate unchanged. Compute benchmarks show 3-5x improvement.

### Phase 4: Runtime Integration (4-8 weeks)

**Goal**: Working event loop and I/O APIs.

This phase is **required** for Otter to be useful as a runtime.

#### 4.1 Event Loop Architecture

**Embedding-first design**: Otter VM does not own the event loop by default. It integrates via adapter trait.

```rust
/// Adapter trait for event loop integration
pub trait EventLoopAdapter: Send + Sync {
    /// Schedule a microtask to run after current task
    fn schedule_microtask(&self, task: Box<dyn FnOnce() + Send>);

    /// Schedule a timer callback
    fn schedule_timer(&self, delay: Duration, callback: Box<dyn FnOnce() + Send>) -> TimerId;

    /// Cancel a scheduled timer
    fn cancel_timer(&self, id: TimerId);

    /// Poll for I/O readiness (returns when work is available or timeout)
    fn poll(&self, timeout: Option<Duration>) -> io::Result<()>;
}
```

**Standalone mode**: Otter CLI uses `TokioEventLoop`, a default adapter implementation.

```
┌─────────────────────────────────────────────────┐
│  Host Application (Embedded Mode)               │
│  ┌───────────────────────────────────────────┐  │
│  │  EventLoopAdapter trait                   │  │
│  │  - Host provides implementation           │  │
│  │  - Otter calls adapter methods            │  │
│  └───────────────────────────────────────────┘  │
│                       │                         │
│  ┌───────────────────────────────────────────┐  │
│  │  Otter VM (JS execution)                  │  │
│  │  - Does NOT own event loop                │  │
│  │  - Calls adapter for scheduling           │  │
│  └───────────────────────────────────────────┘  │
└─────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────┐
│  Standalone Mode (otter CLI)                    │
│  ┌───────────────────────────────────────────┐  │
│  │  TokioEventLoop (default adapter)         │  │
│  │  - Tokio runtime                          │  │
│  │  - io_uring on Linux (optional feature)   │  │
│  └───────────────────────────────────────────┘  │
└─────────────────────────────────────────────────┘
```

#### 4.2 Event Loop Phase Order

Following Node.js semantics for ecosystem compatibility:

```
┌───────────────────────────────┐
│         START ITERATION       │
└─────────────┬─────────────────┘
              ↓
┌─────────────────────────────────┐
│  1. Microtasks (Promise.then)   │ ← Run until queue empty
└─────────────┬───────────────────┘
              ↓
┌─────────────────────────────────┐
│  2. Timers (setTimeout expired) │ ← Run all ready timers
└─────────────┬───────────────────┘
              ↓
┌─────────────────────────────────┐
│  3. Microtasks (again)          │ ← After each timer callback
└─────────────┬───────────────────┘
              ↓
┌─────────────────────────────────┐
│  4. Poll I/O                    │ ← epoll/kqueue/io_uring
└─────────────┬───────────────────┘
              ↓
┌─────────────────────────────────┐
│  5. Check (setImmediate)        │
└─────────────┬───────────────────┘
              ↓
┌─────────────────────────────────┐
│  6. Microtasks (again)          │
└─────────────┬───────────────────┘
              ↓
┌─────────────────────────────────┐
│  7. Close callbacks             │
└─────────────┬───────────────────┘
              ↓
         NEXT ITERATION
```

#### 4.3 Runtime Files

| File | Purpose |
|------|---------|
| `otter-vm-runtime/src/event_loop.rs` | `EventLoopAdapter` trait + `TokioEventLoop` |
| `otter-vm-runtime/src/timer.rs` | Timer heap, setTimeout/setInterval |
| `otter-vm-runtime/src/microtask.rs` | Microtask queue, queueMicrotask |
| `otter-vm-runtime/src/timer.rs` | setTimeout/setInterval scheduling |

**Success metric**: Event loop passes spec compliance tests. Embedding API works for real integration scenarios.

### Phase 5: Advanced Features (2-3 months)

**Goal**: Complete feature set for production use.

| Feature | Notes |
|---------|-------|
| Source maps & debugging | Chrome DevTools Protocol subset |
| WeakRef & FinalizationRegistry | Requires GC integration |
| Intl API (partial) | ICU4X for NumberFormat, DateTimeFormat, Collator |
| OSR + deopt (JIT v2) | On-stack replacement, deopt snapshots |

---

## 5. Detailed Specifications

### 5.1 GC Integration

#### Phase 0: Rooting API

**Invariant**: Any `Gc<T>` reference must either:

1. Be within a `HandleScope` that will root it, OR
2. Be used only within a single operation that cannot trigger GC

```rust
fn example_good(ctx: &mut VmContext) {
    let scope = HandleScope::new(ctx);

    let obj = JsObject::new(ctx);  // Returns Gc<JsObject>
    let handle = scope.root(obj);  // Now safe across GC

    // This call might trigger GC, but handle is safe
    call_js_function(ctx, some_func, &[handle.into()]);
}

fn example_bad(ctx: &mut VmContext) {
    let obj = JsObject::new(ctx);  // Returns Gc<JsObject>

    // DANGER: call_js_function might trigger GC, obj could be invalid!
    call_js_function(ctx, some_func, &[obj.into()]);
}
```

#### Phase 1: Stop-the-World Mark/Sweep

1. **Stop**: Halt interpreter at safepoint
2. **Mark**: Trace from roots (stack, globals, handles)
3. **Sweep**: Free unmarked objects, return memory to allocator
4. **Resume**: Continue execution

**Roots**:

- Call stack (all registers, locals)
- Global object
- All active `Handle<T>` references
- Pending promise reactions
- Active timers (if holding JS callbacks)

#### Phase 2: Incremental Marking

**Write barrier**: When storing reference A→B, mark B gray if A is black.

```rust
fn write_barrier(ctx: &GcContext, container: Gc<T>, reference: Gc<U>) {
    if ctx.is_marking() && ctx.is_black(container) && !ctx.is_marked(reference) {
        ctx.mark_gray(reference);
    }
}
```

**Incremental steps**: Mark a bounded number of objects per interpreter interrupt check.

### 5.2 Inline Caches

#### State Machine

```rust
pub enum InlineCacheState {
    Uninitialized,
    Monomorphic {
        shape_id: ShapeId,
        offset: u32,
        proto_epoch: u32,  // For proto chain invalidation
    },
    Polymorphic {
        entries: ArrayVec<MonoEntry, 4>,
    },
    Megamorphic,
}
```

#### Transition Rules

| Current State | Condition | Next State |
|---------------|-----------|------------|
| Uninitialized | First access | Monomorphic |
| Monomorphic | Same shape | Monomorphic (hit) |
| Monomorphic | Different shape | Polymorphic (2 entries) |
| Polymorphic | Existing shape | Polymorphic (hit) |
| Polymorphic | New shape, < 4 entries | Polymorphic (add entry) |
| Polymorphic | New shape, 4 entries | Megamorphic |
| Any | Shape invalidated | Uninitialized |

#### Epoch Invalidation

Operations that bump the global epoch (invalidating all cached proto chains):

- `Object.defineProperty` on any prototype object
- `__proto__` assignment
- `Object.setPrototypeOf`
- `Object.preventExtensions` / `seal` / `freeze` on prototype

When epoch mismatches, IC transitions to Uninitialized and re-caches.

### 5.3 Type Profiling & Quickening

#### Lightweight Feedback (Phase 1)

```rust
#[repr(u8)]
pub enum TypeTag {
    Undefined = 0x01,
    Null      = 0x02,
    Boolean   = 0x04,
    Int32     = 0x08,
    Number    = 0x10,  // f64 (not i32)
    String    = 0x20,
    Object    = 0x40,
    BigInt    = 0x80,
}

struct TypeFeedback {
    seen: u8,  // Bitmask of TypeTag
}
```

#### Quickening Decision (Phase 2)

```rust
fn should_quicken_to_i32(feedback: &TypeFeedback) -> bool {
    feedback.seen == TypeTag::Int32 as u8
}

fn should_quicken_to_f64(feedback: &TypeFeedback) -> bool {
    feedback.seen & !(TypeTag::Int32 as u8 | TypeTag::Number as u8) == 0
}
```

#### Quickened Instruction Format

```rust
// Original: Add r0, r1, r2
// Quickened: AddI32 r0, r1, r2
//   - If both r1 and r2 are i32: perform i32 add, store i32
//   - If type mismatch: deopt to generic Add, clear quickening
```

### 5.4 Generators

#### Frame Snapshot Format

```rust
pub struct GeneratorFrame {
    // Execution state
    pub pc: usize,
    pub sp: usize,

    // Value state
    pub registers: Box<[Value]>,
    pub accumulator: Value,

    // Control flow state
    pub catch_stack: Vec<CatchEntry>,
    pub finally_stack: Vec<FinallyEntry>,

    // Environment state
    pub environment: Handle<Environment>,
    pub this_binding: Value,
    pub new_target: Option<Value>,

    // Protocol state
    pub pending_throw: Option<Value>,
    pub completion_type: CompletionType,
}

pub enum CompletionType {
    Normal,
    Return(Value),
    Throw(Value),
}
```

#### Protocol Implementation

```rust
impl Generator {
    pub fn next(&mut self, ctx: &mut VmContext, value: Value) -> Result<IteratorResult, VmError> {
        match self.state {
            GeneratorState::Suspended(ref mut frame) => {
                // Resume: put `value` into accumulator, continue from pc
                frame.accumulator = value;
                self.state = GeneratorState::Executing;
                self.run_until_yield_or_return(ctx)
            }
            GeneratorState::Completed => {
                Ok(IteratorResult { value: Value::undefined(), done: true })
            }
            GeneratorState::Executing => {
                Err(VmError::type_error("Generator is already executing"))
            }
        }
    }

    pub fn return_(&mut self, ctx: &mut VmContext, value: Value) -> Result<IteratorResult, VmError> {
        match self.state {
            GeneratorState::Suspended(ref mut frame) => {
                // Set completion type to Return, run finally blocks
                frame.completion_type = CompletionType::Return(value);
                self.state = GeneratorState::Executing;
                self.run_finally_and_complete(ctx)
            }
            GeneratorState::Completed => {
                Ok(IteratorResult { value, done: true })
            }
            GeneratorState::Executing => {
                Err(VmError::type_error("Generator is already executing"))
            }
        }
    }

    pub fn throw(&mut self, ctx: &mut VmContext, error: Value) -> Result<IteratorResult, VmError> {
        match self.state {
            GeneratorState::Suspended(ref mut frame) => {
                // Set pending throw, resume to let catch/finally handle it
                frame.pending_throw = Some(error);
                self.state = GeneratorState::Executing;
                self.run_until_yield_or_return(ctx)
            }
            GeneratorState::Completed => {
                Err(VmError::from_value(error))
            }
            GeneratorState::Executing => {
                Err(VmError::type_error("Generator is already executing"))
            }
        }
    }
}
```

### 5.5 TypedArrays

#### ArrayBuffer

```rust
pub struct ArrayBuffer {
    data: Option<Vec<u8>>,  // None if detached
    max_byte_length: Option<usize>,  // For resizable buffers (ES2024)
}

impl ArrayBuffer {
    pub fn is_detached(&self) -> bool {
        self.data.is_none()
    }

    pub fn detach(&mut self) {
        self.data = None;
    }

    pub fn transfer(self) -> ArrayBuffer {
        ArrayBuffer {
            data: self.data,
            max_byte_length: self.max_byte_length,
        }
    }
}
```

#### TypedArray View

```rust
pub struct TypedArrayView {
    buffer: Handle<ArrayBuffer>,
    byte_offset: usize,
    length: usize,  // Element count, not byte count
    kind: TypedArrayKind,
}

pub enum TypedArrayKind {
    Int8,
    Uint8,
    Uint8Clamped,
    Int16,
    Uint16,
    Int32,
    Uint32,
    Float32,
    Float64,
    BigInt64,
    BigUint64,
}

impl TypedArrayView {
    pub fn get(&self, index: usize) -> Result<Value, VmError> {
        if self.buffer.borrow().is_detached() {
            return Err(VmError::type_error("ArrayBuffer is detached"));
        }
        if index >= self.length {
            return Ok(Value::undefined());
        }
        // Read from buffer at byte_offset + index * element_size
        // ...
    }
}
```

#### SharedArrayBuffer

**Decision**: Defer until Atomics implementation. SharedArrayBuffer without Atomics is useless (no synchronization primitives). Design them together.

### 5.6 RegExp (Safe Mode)

#### Architecture

**Default engine**: `regress` crate (JS-compatible, backtracking)

**NOT replacing with `regex` crate**. The `regex` crate does not support:

- UTF-16 surrogate pairs (JS strings are UTF-16)
- `lastIndex` property (stateful matching)
- Sticky flag (`/y`)
- Global flag iteration semantics
- Capture group numbering matching JS
- Backreferences (`\1`, `\2`)
- Lookahead/lookbehind

#### ReDoS Protection

```rust
pub struct RegExpOptions {
    /// Maximum instructions to execute (prevents ReDoS)
    pub instruction_budget: Option<u64>,

    /// Hard timeout for matching
    pub timeout: Option<Duration>,
}

impl RegExp {
    pub fn exec_with_options(
        &self,
        input: &str,
        options: RegExpOptions,
    ) -> Result<Option<MatchResult>, RegExpError> {
        let mut instruction_count = 0;
        let budget = options.instruction_budget.unwrap_or(u64::MAX);
        let deadline = options.timeout.map(|d| Instant::now() + d);

        // Execute with periodic budget checks
        loop {
            instruction_count += 1;
            if instruction_count > budget {
                return Err(RegExpError::InstructionBudgetExceeded);
            }
            if let Some(d) = deadline {
                if Instant::now() > d {
                    return Err(RegExpError::Timeout);
                }
            }
            // ... matching logic
        }
    }
}
```

#### Safe Subset Engine (Optional, Trusted Patterns Only)

For patterns known to be safe (e.g., compiled into application, not user-provided):

```rust
pub enum RegExpEngine {
    /// Default: regress (full JS compatibility, backtracking)
    Regress(regress::Regex),

    /// Optional: Safe subset using rust regex (O(n) guaranteed)
    /// Only for patterns without: backrefs, lookahead, lookbehind
    SafeSubset(regex::Regex),
}
```

**Never** claim `regex` as "drop-in replacement" or "default" for JS RegExp.

### 5.7 JIT Architecture

#### Prerequisites Checklist

| Prerequisite | Status | Notes |
|--------------|--------|-------|
| Non-moving GC | Phase 1 | Objects don't move, pointers stay valid |
| Conservative stack scan | Phase 1 | No need for precise stack maps |
| Calling convention defined | Phase 3 start | JIT ↔ interpreter interop |
| Safepoint protocol | Phase 3 start | Where GC can interrupt JIT code |
| W^X memory handling | Phase 3 start | Separate RX/RW pages |

#### JIT v1 Design

```rust
pub struct JitCompiler {
    module: JITModule,
    func_ctx: FunctionBuilderContext,
}

impl JitCompiler {
    pub fn compile(&mut self, function: &BytecodeFunction, feedback: &TypeFeedback) -> Result<*const u8, JitError> {
        let mut builder = FunctionBuilder::new(&mut self.func_ctx, ...);

        // Prologue: check type assumptions
        self.emit_type_guards(&mut builder, feedback)?;

        // Body: translate each bytecode instruction
        for instr in function.instructions() {
            self.translate_instruction(&mut builder, instr, feedback)?;
        }

        // Epilogue: return value
        self.emit_return(&mut builder)?;

        // Finalize
        let code = self.module.finalize_function(...)?;
        Ok(code)
    }

    fn emit_type_guards(&mut self, builder: &mut FunctionBuilder, feedback: &TypeFeedback) -> Result<(), JitError> {
        // If feedback says "always i32", emit guard:
        //   if typeof(arg) != i32: goto bailout
        // ...
    }
}
```

#### Bailout (v1: Simple)

On guard failure in JIT code:

1. Call `bailout_to_interpreter(function_id, args...)`
2. Re-execute entire function in interpreter
3. Do NOT attempt to reconstruct mid-function state

This is suboptimal but correct. OSR and deopt snapshots are v2 features.

### 5.8 Peephole Optimizations

#### Safe Optimizations

| Pattern | Replacement | Conditions |
|---------|-------------|------------|
| `LoadConst(0); Add` | (no-op if identity) | Constant is numeric 0 |
| `LoadConst; LoadConst; BinOp` | `LoadConst(result)` | Both constants, pure operation |
| `Dup; Pop` | (remove both) | No side effects between |
| `Jump(next_instruction)` | (remove) | Unconditional jump to next |

#### Unsafe (DO NOT IMPLEMENT)

| Pattern | Why Unsafe |
|---------|------------|
| `x * 2 → x << 1` | Different semantics (ToNumber vs ToInt32), BigInt, valueOf |
| `x / 2 → x >> 1` | Wrong for negative numbers, non-integers |
| `x + "" → String(x)` | Different for Symbols, valueOf/toString order |
| `!!x → Boolean(x)` | Only equivalent for some types |

**Rule**: Only optimize when both operands are compile-time constants with known types, or when runtime type feedback + guards make it safe.

---

## 6. Verification Strategy

### 6.1 Test262 Conformance

**PR checks** (fast, blocking):

- Smoke subset: ~1000 core tests
- Feature-specific directories for changed code
- Target: <5 minutes

**Nightly** (comprehensive):

- Full Test262 run
- Diff report against previous run
- Track pass rate over time

**Feature mapping**:

| Feature | Test262 Directory |
|---------|-------------------|
| Generators | `language/statements/generators/`, `language/expressions/yield/` |
| TypedArrays | `built-ins/TypedArray/`, `built-ins/ArrayBuffer/` |
| Async | `language/expressions/async-*`, `built-ins/Promise/` |

### 6.2 Benchmarks

**Environment controls**:

- Fixed CPU frequency (disable turbo, governor = performance)
- Fixed CPU affinity (pin to specific cores)
- Warm-up runs before measurement
- Multiple runs (minimum 10) with statistical analysis

**Regression detection**:

- Compute mean and standard deviation
- Fail if mean regresses >10% outside normal variance
- Report confidence intervals

**Benchmark suite**:

| Benchmark | What It Tests |
|-----------|---------------|
| `property_access.js` | IC effectiveness |
| `numeric_loops.js` | Type specialization |
| `function_calls.js` | Call overhead |
| `gc_pressure.js` | GC pause times, throughput |
| `string_concat.js` | String performance |

### 6.3 Fuzzing

**Differential fuzzing**:

- Generate random JS programs
- Run in Otter and reference engine (QuickJS or V8)
- Compare outputs and error behavior

**GC stress testing**:

- Force GC at every safepoint
- Verify no use-after-free, no dangling pointers

**JIT security fuzzing** (Phase 3+):

- Generate programs that stress type guards
- Verify guards fire correctly
- Check for memory safety violations

---

## 7. Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| GC migration breaks existing code | Medium | High | Gradual rollout, feature flag, extensive testing |
| Rooting bugs cause use-after-free | Medium | Critical | Conservative rooting, fuzzing, ASAN in CI |
| IC invalidation missed edge case | Medium | Medium | Test262 proto mutation tests, fuzzing |
| JIT type guard bypass | Low | Critical | Fuzzing, code review, sandboxing |
| Performance regression during refactor | Medium | Medium | Continuous benchmarking, rollback capability |
| Timeline slip | High | Medium | Conservative estimates, clear phase gates |

---

## 8. Success Metrics (Phase-Appropriate)

### Phase 0-1 (Correctness)

| Metric | Target | Measurement |
|--------|--------|-------------|
| Circular reference collection | 100% collected | Stress test with known cycles |
| Heap growth under allocation | Bounded (<2x live data) | Long-running allocation test |
| GC correctness under fuzz | No crashes/corruption | 24h fuzz run with ASAN |
| Test262 generators | >95% pass | Test262 subset |
| Test262 TypedArray | >95% pass | Test262 subset |

### Phase 2 (Performance Infrastructure)

| Metric | Target | Measurement |
|--------|--------|-------------|
| Monomorphic property access | >5x improvement | Microbenchmark vs baseline |
| Polymorphic property access | >2x improvement | Microbenchmark vs baseline |
| Quickened numeric loop | >2x improvement | Microbenchmark vs baseline |
| IC hit rate | >80% on typical code | Instrumented runs |

### Phase 3 (JIT)

| Metric | Target | Measurement |
|--------|--------|-------------|
| Test262 pass rate | Unchanged from Phase 2 | Full Test262 run |
| Compute benchmark | 3-5x improvement | Numeric benchmarks |
| Compile time | <10ms per function | Compilation timing |
| JIT code size | <10x bytecode size | Memory measurement |

### Phase 4 (Runtime)

| Metric | Target | Measurement |
|--------|--------|-------------|
| Event loop spec compliance | Passes Node.js compat tests | Test suite |
| Timer accuracy | <1ms deviation | Timer precision tests |
| Embedding API | Works for 2+ real integrations | Integration testing |

---

## Appendix A: Competitive Analysis (Condensed)

### Positioning

Otter targets **embedded scripting** use cases, not competition with V8/JSC on peak throughput.

| Engine | Strength | Otter Comparison |
|--------|----------|------------------|
| V8 | Peak throughput, large team | Otter: Simpler, Rust safety, smaller binary |
| JSC | Mobile efficiency, Bun ecosystem | Otter: Better embedding, no Apple dependency |
| QuickJS | Small size, no JIT | Otter: Better performance with JIT |
| Boa | Rust, pure interpreter | Otter: JIT, more complete builtins |

### Target Use Cases

1. **Scriptable applications**: Game engines, databases, desktop apps
2. **Edge functions**: Serverless with cold start sensitivity
3. **Sandboxed execution**: Untrusted code with capability-based security
4. **Rust integration**: Native Rust applications needing JS scripting

### NOT Competing On

- Long-running server workloads (V8/JSC better)
- Browser compatibility (not a goal)
- Full Node.js API parity (selective compatibility)

---

## Appendix B: Tooling Roadmap (Separate Document)

Tooling is tracked separately and does not block VM development.

| Tool | Status | Priority |
|------|--------|----------|
| Bundler | Not started | Low (after Phase 4) |
| Package manager | Basic (`otter-pm`) | Medium |
| Test runner | Not started | Medium |
| Debugger | Not started | Medium (after source maps) |

See `TOOLING_ROADMAP.md` for details.
