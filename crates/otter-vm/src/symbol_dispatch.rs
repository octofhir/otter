//! `Symbol` namespace static dispatch — `Symbol(...)`, `Symbol.for`,
//! `Symbol.keyFor`, and the well-known accessor table.
//!
//! Mirrors the [`crate::math`] / [`crate::json`] / [`crate::promise_dispatch`]
//! pattern: the runtime exposes two opcodes (`Op::SymbolCall`,
//! `Op::SymbolLoad`) that bottom out in this module so the compiler
//! can lower `Symbol.<name>` syntax directly without a real global.
//!
//! # Contents
//! - [`load_static`] — fetch a well-known symbol value or `prototype`
//!   placeholder reachable through `Symbol.<name>` (read context).
//! - [`call`] — handle `Symbol(...)` (constructor form) and
//!   `Symbol.<method>(args...)` (currently `for` / `keyFor`).
//! - [`SymbolError`] — failure mode the dispatcher converts to
//!   `VmError`.
//!
//! # Invariants
//! - `Symbol(...)` returns a **fresh** primitive symbol per call;
//!   calling with `new` is rejected by the spec but the foundation
//!   does not yet thread that distinction through the runtime, so
//!   the bare-call path is the only supported shape.
//! - Well-known symbol values are returned as a stable singleton
//!   per [`crate::WellKnownSymbols`].
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-symbol-objects>
//! - <https://tc39.es/ecma262/#sec-symbol.for>
//! - <https://tc39.es/ecma262/#sec-symbol.keyfor>

use crate::string::JsString;
use crate::symbol::WellKnown;
use crate::{Interpreter, JsSymbol, Value};

/// Failure modes returned by [`call`] / [`load_static`]. The
/// interpreter mapper widens these to [`crate::VmError`].
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum SymbolError {
    /// Symbol member name is unknown (not a well-known symbol nor a
    /// recognised method).
    #[error("Symbol.{0} is not defined")]
    UnknownMember(String),
    /// Argument was the wrong type for the called member.
    #[error("Symbol.{name}: argument {index} {reason}")]
    BadArgument {
        /// JS-visible member name.
        name: &'static str,
        /// Argument index (0-based).
        index: u16,
        /// Short reason.
        reason: &'static str,
    },
    /// Allocation failed (heap cap exhausted).
    #[error("out of memory: requested {requested_bytes} bytes, heap limit {heap_limit_bytes}")]
    OutOfMemory {
        /// Bytes requested.
        requested_bytes: u64,
        /// Heap cap (`0` = unlimited).
        heap_limit_bytes: u64,
    },
}

impl From<otter_gc::OutOfMemory> for SymbolError {
    fn from(err: otter_gc::OutOfMemory) -> Self {
        Self::OutOfMemory {
            requested_bytes: err.requested_bytes(),
            heap_limit_bytes: err.heap_limit_bytes(),
        }
    }
}

impl From<crate::symbol::SymbolRegistryError> for SymbolError {
    fn from(err: crate::symbol::SymbolRegistryError) -> Self {
        match err {
            crate::symbol::SymbolRegistryError::OutOfMemory(e) => e.into(),
        }
    }
}

/// `Symbol.<name>` static read.
///
/// # Algorithm
/// 1. If `name` is a well-known tag (`iterator`, `toPrimitive`, …)
///    return the per-interpreter singleton.
/// 2. Otherwise raise [`SymbolError::UnknownMember`].
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-well-known-symbols>
pub fn load_static(interp: &mut Interpreter, name: &str) -> Result<Value, SymbolError> {
    // Try to fetch the named own property from the realm-installed
    // Symbol constructor first. `Symbol.prototype`, `Symbol.for`,
    // `Symbol.keyFor`, plus every well-known own-data symbol slot
    // (`Symbol.iterator`, `Symbol.toPrimitive`, …) resolves here.
    let symbol_ctor = crate::object::get(interp.global_this, &interp.gc_heap, "Symbol");
    match &symbol_ctor {
        Some(Value::Object(ctor)) => {
            if let Some(value) = crate::object::get(*ctor, &interp.gc_heap, name) {
                return Ok(value);
            }
        }
        Some(Value::NativeFunction(native)) => {
            if let Ok(Some(desc)) = native.own_property_descriptor(&mut interp.gc_heap, name)
                && let crate::object::DescriptorKind::Data { value } = desc.kind
            {
                return Ok(value);
            }
        }
        _ => {}
    }
    // Compiler-fast-path well-known symbols even when bootstrap has
    // not yet wired the constructor (eg. during early
    // initialisation) — keeps the typed `Op::SymbolLoad` opcode
    // working before `install_symbol_well_knowns_post_bootstrap` runs.
    if let Some(tag) = WellKnown::from_name(name) {
        return Ok(Value::symbol(interp.well_known_symbols().get(tag)));
    }
    Err(SymbolError::UnknownMember(name.to_string()))
}

/// Sentinel name used by the compiler when lowering a bare
/// `Symbol(...)` call (no method member). Empty string is reserved
/// since it would be a syntax error as a property name.
pub const CONSTRUCTOR_SENTINEL: &str = "";

/// Dispatch `Symbol(desc)` ([`SymbolMethod::Construct`]) or
/// `Symbol.<method>(args...)`. Routes the typed
/// [`SymbolMethod`] emitted by the compiler.
pub fn call(
    interp: &mut Interpreter,
    method: otter_bytecode::method_id::SymbolMethod,
    args: &[Value],
) -> Result<Value, SymbolError> {
    use otter_bytecode::method_id::SymbolMethod as M;
    match method {
        // Bare `Symbol(desc)` — fresh primitive symbol per call.
        // Spec §20.4.1.1.
        M::Construct => construct_symbol(interp, args),
        M::For => symbol_for(interp, args),
        M::KeyFor => symbol_key_for(interp, args),
    }
}

/// `Symbol(desc)` — Spec §20.4.1.1 (called as a function, not
/// `new Symbol(desc)`). Returns a fresh primitive symbol; spec
/// rejects the `new` form (TypeError) but the foundation has no
/// dedicated path for that today.
fn construct_symbol(interp: &mut Interpreter, args: &[Value]) -> Result<Value, SymbolError> {
    let description = match args.first() {
        None | Some(Value::Undefined) => None,
        Some(Value::String(s)) => Some(*s),
        // §7.1.17 ToString rejects Symbol with TypeError.
        Some(Value::Symbol(_)) => {
            return Err(SymbolError::BadArgument {
                name: "Symbol",
                index: 0,
                reason: "Cannot convert a Symbol value to a string",
            });
        }
        // Spec coerces to string with `ToString`. Foundation
        // narrows to non-Symbol primitives plus a fallback to
        // `display_string` for Object operands (no `@@toPrimitive` /
        // `toString` invocation here — the `Op::SymbolCall`
        // dispatcher has no `ExecutionContext`).
        Some(other) => {
            let rendered =
                JsString::from_str(&other.display_string(&interp.gc_heap), interp.gc_heap_mut())?;
            Some(rendered)
        }
    };
    let sym = JsSymbol::new(&mut interp.gc_heap, description)?;
    Ok(Value::symbol(sym))
}

/// `Symbol.for(key)` — Spec §20.4.2.4. Coerces `key` to a string
/// (spec uses `ToString`) and looks up / inserts in the registry.
fn symbol_for(interp: &mut Interpreter, args: &[Value]) -> Result<Value, SymbolError> {
    let key = key_argument(args, &interp.gc_heap, "for")?;
    let sym = interp.symbol_for_key(&key)?;
    Ok(Value::symbol(sym))
}

/// `Symbol.keyFor(sym)` — Spec §20.4.2.6.
fn symbol_key_for(interp: &mut Interpreter, args: &[Value]) -> Result<Value, SymbolError> {
    let sym = match args.first() {
        Some(Value::Symbol(s)) => s,
        _ => {
            return Err(SymbolError::BadArgument {
                name: "keyFor",
                index: 0,
                reason: "must be a symbol",
            });
        }
    };
    match interp.symbol_registry().key_for(sym) {
        Some(key) => {
            let s = JsString::from_str(&key, interp.gc_heap_mut())?;
            Ok(Value::string(s))
        }
        None => Ok(Value::undefined()),
    }
}

/// Coerce the first argument of a registry call to a Rust string.
/// Spec invokes ToString; foundation accepts strings and
/// non-`Symbol` primitives directly.
fn key_argument(
    args: &[Value],
    heap: &otter_gc::GcHeap,
    name: &'static str,
) -> Result<String, SymbolError> {
    match args.first() {
        None | Some(Value::Undefined) => Ok("undefined".to_string()),
        Some(Value::String(s)) => Ok(s.to_lossy_string(heap)),
        Some(Value::Symbol(_)) => Err(SymbolError::BadArgument {
            name,
            index: 0,
            reason: "key must not be a symbol",
        }),
        Some(other) => Ok(other.display_string(heap)),
    }
}
