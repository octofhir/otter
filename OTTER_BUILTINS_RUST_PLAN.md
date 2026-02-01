# Otter: Rust Intrinsics — Status & Remaining Work

Status: **builtins.js REMOVED** — all core ECMAScript intrinsics are Rust-native.

## Architecture

All builtins live in `crates/otter-vm-core/src/intrinsics.rs` + `intrinsics_impl/` modules:

| Module | Lines | Scope |
|--------|-------|-------|
| `helpers.rs` | ~50 | Utility functions (strict_equal, same_value_zero) |
| `array.rs` | ~1300 | Array.prototype (all methods incl. callbacks + iterators) |
| `boolean.rs` | ~140 | Boolean constructor + prototype |
| `date.rs` | ~970 | ALL Date.prototype methods (ES2026) |
| `function.rs` | ~260 | Function.prototype (call, apply, bind, toString) |
| `map_set.rs` | ~1650 | Map/Set/WeakMap/WeakSet + ES2025 set algebra + iterators |
| `math.rs` | ~470 | Math (8 constants + 37 methods, ES2026) |
| `number.rs` | ~420 | Number.prototype methods |
| `promise.rs` | ~550 | Promise constructor + statics + prototype |
| `reflect.rs` | ~430 | Reflect (all 13 ES2015+ methods) |
| `regexp.rs` | ~700 | RegExp constructor + all prototype methods |
| `string.rs` | ~990 | String.prototype methods + Symbol.iterator |
| `temporal.rs` | ~150 | Temporal namespace |
| `generator.rs` | ~280 | Generator.prototype + AsyncGenerator.prototype (ES2026) |
| `typed_array.rs` | ~850 | TypedArray.prototype + all 11 typed array types (ES2026) |
| `proxy.rs` | ~150 | Proxy constructor + Proxy.revocable() |
| `proxy_operations.rs` | ~1050 | All 13 ES2026 proxy handler traps + invariant validation (ES §9.5) |
| `object.rs` | ~1025 | Object.prototype (5 methods) + Object static methods (21 methods) |
| `error.rs` | ~233 | Error.prototype + 7 error types + stack trace support |

**Core file**: `intrinsics.rs` (~1285 LOC) — orchestrates initialization in 4 stages:

Intrinsics initialization: 4 stages in `intrinsics.rs`:
1. `allocate()` — pre-allocate all prototype objects + well-known symbols
2. `wire_prototype_chains()` — set up [[Prototype]] links
3. `init_core()` — initialize all methods with correct property attributes
4. `install_on_global()` — install constructors on global object

Callback dispatch uses `InterceptionSignal` enum: native function detects closure callback, returns interception signal, interpreter catches it and calls `call_function()` in a loop with full VM context.

## Remaining Work

### Completed Recently

✅ **Object and Error intrinsics extraction + Stack Trace support** (2026-02-01) — Extracted Object and Error implementations from monolithic `intrinsics.rs` into dedicated modules. Includes:
  - **`object.rs`** (~1025 LOC): All Object.prototype methods (toString, valueOf, hasOwnProperty, isPrototypeOf, propertyIsEnumerable) + 21 Object static methods (getPrototypeOf, setPrototypeOf, keys, values, entries, assign, freeze, seal, create, defineProperty, etc.)
  - **`error.rs`** (~233 LOC): Error.prototype + all 7 error types (Error, TypeError, RangeError, ReferenceError, SyntaxError, URIError, EvalError)
  - **Stack trace capture**: Automatic stack trace capture for all Error objects during construction
  - **Error.prototype.stack getter**: Lazy formatting of stack traces with error name, message, and call frames
  - **Inheritance support**: `class CustomError extends Error` automatically captures stack traces via super() calls
  - **Implementation**: Stack capture in `Instruction::Construct` handler (interpreter.rs), triggered by `__is_error__` marker on Error prototypes
  - **Impact**: `intrinsics.rs` reduced from 2353 to 1285 LOC (−1068 LOC, −45%)
  - **All tests passing**: 155 unit tests + manual stack trace validation

✅ **TypedArray implementation** (2026-02-01) — Complete ES2026-compliant TypedArray implementation with all 11 types (Int8Array through BigUint64Array). Includes:
  - Full intrinsics system integration with %TypedArray%.prototype and 11 specific prototypes
  - All constructors supporting 5 forms: empty, length, typedArray, buffer view, arrayLike
  - Static methods: BYTES_PER_ELEMENT, from(), of()
  - All prototype methods: at, copyWithin, fill, includes, indexOf, join, lastIndexOf, reverse, set, slice, subarray, toString, toLocaleString
  - Getters: buffer, byteLength, byteOffset, length
  - Iterators: values, keys, entries, Symbol.iterator
  - Proper object-based storage with hidden __TypedArrayData__ property for getter/method access

✅ **Generator/AsyncGenerator prototypes** (2026-02-01) — `Generator.prototype` and `AsyncGenerator.prototype` fully implemented as proper intrinsics with ES2026-compliant prototype chains, all methods (next, return, throw), Symbol.iterator/asyncIterator, and Symbol.toStringTag.

✅ **Iterator protocol completeness** (2026-02-01) — Map/Set/String iterators fully implemented with `%IteratorPrototype%` chain, Symbol.iterator support, UTF-16 surrogate pair handling for strings, and snapshot semantics for stable iteration.

✅ **Proxy handler traps** (2026-02-01) — Complete ES2026-compliant Proxy implementation with all 13 handler traps. Includes:
  - **Phase 1**: get, set traps with invariant validation for non-configurable/non-writable properties
  - **Phase 2**: has, deleteProperty, ownKeys, getOwnPropertyDescriptor, defineProperty traps with non-extensible target validation
  - **Phase 3**: getPrototypeOf, setPrototypeOf, isExtensible, preventExtensions traps with extensibility invariants
  - **Phase 4**: apply, construct traps for function call/constructor interception
  - New module `proxy_operations.rs` (~1050 LOC) with all trap implementations and invariant checks
  - InterceptionSignal pattern for Reflect.* methods (10 new signals: ReflectGetProxy, ReflectSetProxy, etc.)
  - Full integration with interpreter instructions (GetProp, SetProp, DeleteProp, In, Call, Construct)
  - ES §9.5.1-14 spec compliance with proper error messages for invariant violations
  - Revoked proxy handling, proxy chains support, and proper trap invocation with handler as 'this'

### Medium Priority

(No items)

### Low Priority

(No items)

## Task 2: Runtime Job/Microtask Drain Correctness

Once builtins are stable, tighten microtask drain semantics:

- Drain microtasks/jobs after synchronous script execution, module evaluation, and Promise resolution.
- Define a single "drain jobs" loop called at well-defined points.
- Location: `crates/otter-vm-runtime/src/otter_runtime.rs`

## Appendix: Test262 Runner Improvements

Delegated to another agent. Key gaps in `crates/otter-test262`:

- `flags: [async]` handling — intercept `print()` for `"Test262:AsyncTestComplete"` (Boa style)
- `flags: [module]` handling — run as module, not script
- Negative expectations — honor `negative.phase` and validate error type
- `$262` host object — `createRealm`, `evalScript`, `detachArrayBuffer`, `gc`
