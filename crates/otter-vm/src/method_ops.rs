//! Method-call opcode helpers.
//!
//! `CallMethodValue` is the widest dynamic dispatch opcode in the interpreter:
//! it handles prototype intrinsics, generator and iterator helpers, collection
//! callbacks, object/function prototype intercepts, and ordinary property
//! method lookup before falling into the shared callable path.
//!
//! # Contents
//! - `CallMethodValue` executable operand decoding.
//! - Callback-driven Array prototype methods.
//!
//! # Invariants
//! - Stack-modifying callback paths run before the dense in-frame match.
//! - Caller PC is advanced before synchronous callback dispatch where nested
//!   execution can re-enter the VM.
//! - Ordinary method lookup still funnels into `Interpreter::invoke`.
//!
//! # See also
//! - [`crate::call_ops`]
//! - [`crate::executable`]

use crate::{call_ops::LeanCallbackState, holt_stack::HoltStack};
use otter_bytecode::Operand;
use smallvec::SmallVec;

use crate::function_ops::BindMetadataGet;
use crate::native_abi::RuntimeStubId;
use crate::{
    ExecutionContext, GeneratorResumeKind, Interpreter, JsString, NumberValue, PendingBindFunction,
    PendingBindStage, Value, VmError, VmGetOutcome, VmPropertyKey, bigint,
    boolean::prototype as boolean_prototype,
    bootstrap_collections, collections_prototype, date, descriptor_value, function_metadata, math,
    native_function::VmIntrinsicFunction,
    number,
    operand_decode::{const_operand, register_operand},
    promise_dispatch,
    property_atom::AtomizedPropertyKey,
    property_ic::{LoadPropertyIc, PropertyIcKind},
    read_register, regexp_prototype, require_callable,
    string::prototype as string_prototype,
    symbol_prototype, weak_refs, write_register,
};

/// Root set for the fast `Array.prototype.*` dispatch path: the live
/// interpreter roots plus the receiver and argument values, which would
/// otherwise be invisible to a scavenge triggered inside the array method
/// (they live only in this stack frame, not in a published VM frame). Mirrors
/// [`crate::call_ops`]'s native-call root scope.
struct ArrayFastDispatchRoots<'a> {
    interp_roots: otter_gc::ExtraRoots,
    recv: Value,
    args: &'a [Value],
}

impl otter_gc::ExtraRootSource for ArrayFastDispatchRoots<'_> {
    fn visit_extra_roots(&self, visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)) {
        self.interp_roots.visit(visitor);
        self.recv.trace_value_slots(visitor);
        for value in self.args {
            value.trace_value_slots(visitor);
        }
    }
}

/// Root scope for the direct Map/Set builtin dispatch. The receiver handle and
/// the argument values live only in this native stack frame, so a scavenge
/// triggered by an inserting `set` / `add` would otherwise miss them; rooting
/// `recv` also lets the caller read the forwarded receiver back after the call
/// (the spec returns the collection from `Map.prototype.set` / `Set.prototype.add`).
struct CollectionFastDispatchRoots<'a> {
    interp_roots: otter_gc::ExtraRoots,
    recv: Value,
    args: &'a [Value],
}

impl otter_gc::ExtraRootSource for CollectionFastDispatchRoots<'_> {
    fn visit_extra_roots(&self, visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)) {
        self.interp_roots.visit(visitor);
        self.recv.trace_value_slots(visitor);
        for value in self.args {
            value.trace_value_slots(visitor);
        }
    }
}

/// `true` when a collection's recorded `[[Prototype]]` is the canonical realm
/// prototype: either implicit (`None` — the default) or an explicit override
/// that still points at `expected`. Any other object is a user-installed
/// prototype that could shadow the builtin, so the direct dispatch bails.
fn prototype_override_is(override_value: Option<Value>, expected: crate::object::JsObject) -> bool {
    match override_value {
        None => true,
        Some(value) => value.as_object() == Some(expected),
    }
}

/// Which Map/Set builtin the direct dispatch resolved to.
#[derive(Clone, Copy)]
enum CollectionFastOp {
    MapGet,
    MapSet,
    MapHas,
    MapDelete,
    SetAdd,
    SetHas,
    SetDelete,
}

impl CollectionFastOp {
    fn from_map_name(name: &str) -> Option<Self> {
        match name {
            "get" => Some(Self::MapGet),
            "set" => Some(Self::MapSet),
            "has" => Some(Self::MapHas),
            "delete" => Some(Self::MapDelete),
            _ => None,
        }
    }

    fn from_set_name(name: &str) -> Option<Self> {
        match name {
            "add" => Some(Self::SetAdd),
            "has" => Some(Self::SetHas),
            "delete" => Some(Self::SetDelete),
            _ => None,
        }
    }

    fn is_map(self) -> bool {
        matches!(
            self,
            Self::MapGet | Self::MapSet | Self::MapHas | Self::MapDelete
        )
    }

    fn is_set(self) -> bool {
        matches!(self, Self::SetAdd | Self::SetHas | Self::SetDelete)
    }

    fn name(self) -> &'static str {
        match self {
            Self::MapGet => "get",
            Self::MapSet => "set",
            Self::MapHas => "has",
            Self::MapDelete => "delete",
            Self::SetAdd => "add",
            Self::SetHas => "has",
            Self::SetDelete => "delete",
        }
    }

    fn matches_builtin(self, method: Value, heap: &otter_gc::GcHeap) -> bool {
        if self.is_map() {
            crate::bootstrap_collections::is_map_prototype_builtin(method, heap, self.name())
        } else {
            crate::bootstrap_collections::is_set_prototype_builtin(method, heap, self.name())
        }
    }

    fn leaf_stub_id(self) -> Option<RuntimeStubId> {
        match self {
            Self::MapGet => Some(crate::native_abi::STUB_COLLECTION_MAP_GET_LEAF.id),
            Self::MapHas => Some(crate::native_abi::STUB_COLLECTION_MAP_HAS_LEAF.id),
            Self::SetHas => Some(crate::native_abi::STUB_COLLECTION_SET_HAS_LEAF.id),
            Self::MapSet | Self::MapDelete | Self::SetAdd | Self::SetDelete => None,
        }
    }

    fn alloc_stub_id(self) -> Option<RuntimeStubId> {
        match self {
            Self::MapGet => Some(crate::native_abi::STUB_COLLECTION_MAP_GET_ALLOC.id),
            Self::MapHas => Some(crate::native_abi::STUB_COLLECTION_MAP_HAS_ALLOC.id),
            Self::MapSet => Some(crate::native_abi::STUB_COLLECTION_MAP_SET_ALLOC.id),
            Self::SetAdd => Some(crate::native_abi::STUB_COLLECTION_SET_ADD_ALLOC.id),
            Self::SetHas => Some(crate::native_abi::STUB_COLLECTION_SET_HAS_ALLOC.id),
            Self::MapDelete | Self::SetDelete => None,
        }
    }
}

/// Resolved collection builtin target carried by method-call feedback.
#[derive(Clone, Copy)]
struct CollectionFastTarget {
    op: CollectionFastOp,
    leaf_stub_id: Option<RuntimeStubId>,
}

impl CollectionFastTarget {
    fn new(op: CollectionFastOp) -> Self {
        Self {
            op,
            leaf_stub_id: op.leaf_stub_id(),
        }
    }
}

/// Monomorphic method-call inline cache entry.
///
/// Entries keep only non-GC metadata: prototype shape, prototype slot, and a
/// stable builtin tag/op. The hot guard re-reads the slot from the realm
/// prototype and validates the builtin by native function identity.
#[derive(Clone, Copy)]
pub(crate) enum MethodCallIc {
    Array(ArrayMethodCallIc),
    Collection(CollectionMethodCallIc),
}

/// Monomorphic method-call inline cache entry for a dense-array builtin site.
///
/// Records the `%Array.prototype%` shape and the own-slot offset that resolved
/// `tag`'s method, so a re-validating guard reads the slot directly (no key
/// hash) and confirms it still holds the original builtin by function pointer.
/// Holds no GC pointer — `proto_shape`/`proto_slot` are plain metadata and the
/// builtin identity is checked against a stable native `fn` address — so the
/// cache needs no tracing and can never dangle across a scavenge.
#[derive(Clone, Copy)]
pub(crate) struct ArrayMethodCallIc {
    proto_shape: crate::object::ShapeId,
    proto_slot: u16,
    tag: crate::array_prototype::ArrayMethodTag,
}

/// Monomorphic method-call inline cache entry for a Map/Set builtin site.
///
/// The receiver family and prototype/expando guards are checked before the
/// cached slot is trusted. Shape + slot are enough to skip the prototype slot
/// lookup and method-name dispatch on the steady-state hot path.
#[derive(Clone, Copy)]
pub(crate) struct CollectionMethodCallIc {
    proto_shape: crate::object::ShapeId,
    proto_slot: u16,
    op: CollectionFastOp,
    leaf_stub_id: Option<RuntimeStubId>,
    alloc_stub_id: Option<RuntimeStubId>,
}

/// Clamp a `ToIntegerOrInfinity` result to an absolute index within
/// `[0, len]` per the relative-index convention shared by §23.2.3
/// `slice` / `subarray` (negative counts from the end, `±Infinity`
/// saturate to the bounds).
fn relative_index_clamp(relative: f64, len: i64) -> i64 {
    if relative < 0.0 {
        let v = len as f64 + relative;
        if v < 0.0 { 0 } else { v as i64 }
    } else {
        relative.min(len as f64) as i64
    }
}

fn iterator_dispatch_method_name(name: &str) -> bool {
    matches!(
        name,
        "map"
            | "filter"
            | "take"
            | "drop"
            | "flatMap"
            | "toArray"
            | "forEach"
            | "reduce"
            | "some"
            | "every"
            | "find"
            | "next"
            | "return"
            | "throw"
    )
}

fn object_prototype_dispatch_method_name(name: &str) -> bool {
    matches!(
        name,
        "hasOwnProperty" | "propertyIsEnumerable" | "isPrototypeOf" | "toString" | "valueOf"
    )
}

fn function_prototype_intrinsic_name(name: &str) -> bool {
    matches!(name, "call" | "apply" | "bind" | "toString")
}

fn function_prototype_intrinsic(name: &str) -> VmIntrinsicFunction {
    match name {
        "call" => VmIntrinsicFunction::FunctionPrototypeCall,
        "apply" => VmIntrinsicFunction::FunctionPrototypeApply,
        "bind" => VmIntrinsicFunction::FunctionPrototypeBind,
        "toString" => VmIntrinsicFunction::FunctionPrototypeToString,
        _ => unreachable!("guarded by function_prototype_intrinsic_name"),
    }
}

fn is_function_prototype_intrinsic_value(
    value: Value,
    heap: &otter_gc::GcHeap,
    intrinsic: VmIntrinsicFunction,
) -> bool {
    value
        .as_native_function()
        .is_some_and(|native| native.is_vm_intrinsic(heap, intrinsic))
        || value
            .as_object()
            .and_then(|obj| crate::object::call_native(obj, heap))
            .and_then(|native_value| native_value.as_native_function())
            .is_some_and(|native| native.is_vm_intrinsic(heap, intrinsic))
}

impl Interpreter {
    /// Handle guarded `Math.<method>(args...)` intrinsic calls.
    pub(crate) fn do_math_call(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let method_id = const_operand(operands.get(1))?;
        let method = otter_bytecode::method_id::MathMethod::from_u32(method_id)
            .ok_or(VmError::InvalidOperand)?;
        let argc = match operands.get(2) {
            Some(&Operand::ConstIndex(n)) => n as usize,
            _ => return Err(VmError::InvalidOperand),
        };
        let caller_byte_len = self.current_byte_len;
        let top_idx = stack.len() - 1;
        let mut arg_values: SmallVec<[Value; 8]> = SmallVec::with_capacity(argc);
        for i in 0..argc {
            let r = register_operand(operands.get(3 + i))?;
            arg_values.push(*read_register(&stack[top_idx], r)?);
        }

        let lexical_math = self.read_global_lexical("Math")?;
        if lexical_math.is_none()
            && let Some(_math_obj) =
                math::original_method_receiver(self.global_this, &self.gc_heap, method)
            && math::args_skip_to_primitive(&arg_values)
        {
            let value =
                math::call(method, &arg_values, &self.gc_heap).map_err(|err| match err {
                    math::MathError::UnknownMember(member) => {
                        self.err_unknown_intrinsic(format!("Math.{member}").into())
                    }
                    math::MathError::BadArgument { reason, .. } => {
                        self.err_type((format!("Math.{} {reason}", method.name())).into())
                    }
                })?;
            write_register(&mut stack[top_idx], dst, value)?;
            stack[top_idx].advance_pc(caller_byte_len)?;
            return Ok(());
        }

        let math_value = if let Some(value) = lexical_math {
            value
        } else {
            let receiver = Value::object(self.global_this);
            let key = VmPropertyKey::String("Math");
            if !self.ordinary_has_property_value(context, receiver, &key, 0)? {
                return Err(self.err_undefined_ident(("Math".to_string()).into()));
            }
            match self.ordinary_get_value(context, receiver, receiver, &key, 0)? {
                VmGetOutcome::Value(value) => value,
                VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync(context, &getter, receiver, SmallVec::new())?
                }
            }
        };
        if math_value.is_nullish() {
            let label = if math_value.is_null() {
                "null"
            } else {
                "undefined"
            };
            return Err(self.err_type((format!("Cannot read properties of {label}")).into()));
        }
        let callee = self
            .get_method_value_for_call(context, stack, math_value, method.name())?
            .unwrap_or_else(Value::undefined);
        if !self.is_callable_runtime(&callee) {
            return Err(VmError::NotCallable);
        }
        stack[top_idx].advance_pc(caller_byte_len)?;
        self.invoke(stack, context, &callee, math_value, arg_values, dst)
    }

    /// §22.1.3 — pre-coerce the arguments of a `String.prototype`
    /// method in place: index-like operands run full `ToNumber`
    /// (`ToIntegerOrInfinity`'s first step, so Symbol / BigInt raise
    /// TypeError at the right slot and user `@@toPrimitive` / `valueOf`
    /// fire), and string operands run `ToPrimitive(String)`. Shared by
    /// the primitive-string fast path in `do_call_method_value` and the
    /// `.call` / property bridge so both invocation styles coerce
    /// identically. A `RegExp` argument to `match` / `matchAll` /
    /// `search` / `normalize` passes through unchanged for its
    /// `@@`-method.
    pub(crate) fn coerce_string_method_args(
        &mut self,
        context: &ExecutionContext,
        name: &str,
        args: &mut [Value],
    ) -> Result<(), VmError> {
        // Each entry is `(arg index, is_int)` in the exact order the spec
        // coerces the operands, so observable side effects (and abrupt
        // completions) fire in spec order — e.g. `lastIndexOf` runs
        // ToString(searchString=arg0) before ToNumber(position=arg1)
        // (§22.1.3.9 steps 3-4), whereas `split` coerces ToUint32(limit=
        // arg1) before ToString(separator=arg0) (§22.1.3.21 steps 6-7).
        let order: &[(usize, bool)] = match name {
            "indexOf" | "lastIndexOf" => &[(0, false), (1, true)],
            // includes/startsWith/endsWith must run IsRegExp(searchString)
            // (and throw) before ToString, so the raw search argument has
            // to reach the impl uncoerced; only the position is pre-coerced.
            "includes" | "startsWith" | "endsWith" => &[(1, true)],
            "slice" | "substring" | "substr" => &[(0, true), (1, true)],
            "at" | "charAt" | "charCodeAt" | "codePointAt" => &[(0, true)],
            "repeat" => &[(0, true)],
            "padStart" | "padEnd" => &[(0, true), (1, false)],
            "replace" | "replaceAll" => &[(0, false)],
            "split" => &[(1, true), (0, false)],
            "concat" => &[(0, false), (1, false), (2, false), (3, false)],
            "match" | "matchAll" | "search" | "normalize" => &[(0, false)],
            // §22.1.3.10 step 3 — `That = ? ToString(that)`.
            "localeCompare" => &[(0, false)],
            "anchor" | "fontcolor" | "fontsize" | "link" => &[(0, false)],
            _ => &[],
        };
        if order.is_empty() {
            return Ok(());
        }
        // Only the `@@match` / `@@matchAll` / `@@search` dispatchers keep
        // a RegExp argument un-stringified; `normalize` ToStrings every
        // form operand (§22.1.3.13 step 3) and then rejects `"/re/"`
        // with the step-4 RangeError.
        let regexp_pass_through = matches!(name, "match" | "matchAll" | "search");
        let is_non_primitive = |v: &Value| {
            v.is_object()
                || v.is_array()
                || v.is_function()
                || v.is_closure()
                || v.is_native_function()
                || v.is_bound_function()
                || v.is_class_constructor()
                || v.is_proxy()
                || (!regexp_pass_through && v.is_regexp())
        };
        for &(idx, is_int) in order {
            let Some(&v) = args.get(idx) else {
                continue;
            };
            if is_int {
                // Skip primitives the native method body already
                // recognises (`undefined` is the "absent" sentinel some
                // §B.2.3.1 substr-style methods key on).
                if v.is_number() || v.is_boolean() || v.is_null() || v.is_undefined() {
                    continue;
                }
                let coerced = self.coerce_to_number(context, &v)?;
                args[idx] = Value::number(coerced);
            } else {
                if !is_non_primitive(&v) {
                    continue;
                }
                let primitive = self.evaluate_to_primitive(
                    context,
                    &v,
                    crate::abstract_ops::ToPrimitiveHint::String,
                )?;
                args[idx] = primitive;
            }
        }
        Ok(())
    }

    /// Handle `Op::CallMethodValue`: the universal method-call op.
    /// Branches by receiver kind:
    /// - Builtin prototype methods — synchronous native dispatch.
    ///   Result lands in the destination register without pushing a
    ///   frame.
    /// - `Object` — load the property; raise `NotCallable` if the
    ///   resolved value is not a function; otherwise call it with
    ///   `this = receiver`.
    /// - `Function` / `Closure` / `BoundFunction` — only the
    ///   `call`, `apply`, and `bind` shapes are recognised; anything
    ///   else surfaces as `UnknownIntrinsic`.
    pub(crate) fn do_call_method_value(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let recv_reg = register_operand(operands.get(1))?;
        let name_idx = const_operand(operands.get(2))?;
        let argc = match operands.get(3) {
            Some(&Operand::ConstIndex(n)) => n as usize,
            _ => return Err(VmError::InvalidOperand),
        };
        let caller_byte_len = self.current_byte_len;
        let top_idx = stack.len() - 1;
        if let Some(result) = self.continue_pending_bind_function(stack, context, dst) {
            return result;
        }
        let recv_value = *read_register(&stack[top_idx], recv_reg)?;
        let mut arg_values: SmallVec<[Value; 8]> = SmallVec::with_capacity(argc);
        for i in 0..argc {
            let r = register_operand(operands.get(4 + i))?;
            arg_values.push(*read_register(&stack[top_idx], r)?);
        }
        if recv_value.is_nullish() {
            let label = if recv_value.is_null() {
                "null"
            } else {
                "undefined"
            };
            return Err(self.err_type((format!("Cannot read properties of {label}")).into()));
        }
        // Ordinary dense array whose `%Array.prototype%` slot is untouched —
        // dispatch the builtin directly (see `try_fast_array_proto_method`).
        // Resolving the call-site IC id here installs the same cache the JIT
        // method-call stub reads (shared site space).
        let method_site = context
            .property_ic_site(stack[top_idx].function_id, stack[top_idx].pc)
            .unwrap_or(usize::MAX);
        if let Some(result) =
            self.try_array_method_call_ic(context, method_site, recv_value, arg_values.as_slice())
        {
            let value = result?;
            stack[top_idx].advance_pc(caller_byte_len)?;
            write_register(&mut stack[top_idx], dst, value)?;
            return Ok(());
        }
        if let Some(result) =
            self.try_collection_method_call_ic(method_site, recv_value, arg_values.as_slice())
        {
            let value = result?;
            stack[top_idx].advance_pc(caller_byte_len)?;
            write_register(&mut stack[top_idx], dst, value)?;
            return Ok(());
        }
        let name = context
            .string_constant_str_for_function(stack[top_idx].function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        if let Some(result) = self.try_fast_array_proto_method(
            context,
            method_site,
            recv_value,
            name,
            arg_values.as_slice(),
        ) {
            let value = result?;
            stack[top_idx].advance_pc(caller_byte_len)?;
            write_register(&mut stack[top_idx], dst, value)?;
            return Ok(());
        }
        // Ordinary Map/Set whose realm prototype slot is the untouched builtin —
        // dispatch the collection primitive directly, skipping both the
        // method-resolution walk and the native call bridge.
        if let Some(result) = self.try_fast_collection_proto_method(
            method_site,
            recv_value,
            name,
            arg_values.as_slice(),
        ) {
            let value = result?;
            stack[top_idx].advance_pc(caller_byte_len)?;
            write_register(&mut stack[top_idx], dst, value)?;
            return Ok(());
        }
        // Method-resolution inline cache. An ordinary object's method is a data
        // slot on its own object or its prototype, so the receiver shape keys
        // the resolved method exactly like a `LoadProperty`. On a hit (or a
        // freshly installed monomorphic candidate) the per-call string `[[Get]]`
        // walk — `has_plain_builtin_method` + `ordinary_get_value` + atom
        // comparison up the chain — is skipped entirely. A non-cacheable
        // shape (accessor method, deep prototype, absent) returns `None` and
        // falls through to the full resolution below.
        if let Some(obj) = recv_value.as_object()
            && let Some(atomized_key) =
                context.property_atom_for_function(stack[top_idx].function_id, name_idx)
            && let Some(site) =
                context.property_ic_site(stack[top_idx].function_id, stack[top_idx].pc)
            && let Some(method) = self.resolve_method_ic(obj, atomized_key, site)
            && self.is_callable_runtime(&method)
        {
            stack[top_idx].advance_pc(caller_byte_len)?;
            return self.invoke(stack, context, &method, recv_value, arg_values, dst);
        }

        if recv_value.is_set() && bootstrap_collections::is_set_method_name(name) {
            let method = self
                .get_method_value_for_call(context, stack, recv_value, name)?
                .unwrap_or_else(Value::undefined);
            if !self.is_callable_runtime(&method) {
                return Err(VmError::NotCallable);
            }
            stack[top_idx].advance_pc(caller_byte_len)?;
            return self.invoke(stack, context, &method, recv_value, arg_values, dst);
        }

        // Iterator-helpers / generator resumption methods always
        // dispatch through the resolved prototype method (native or
        // user override) so one implementation — the §27.1 natives —
        // owns the spec semantics (GetIteratorDirect caching,
        // IteratorClose forwarding, argument-validation order).
        // Generator `next` / `return` / `throw` carry a resumption
        // argument the resume block below threads into the suspended
        // frame; only user (non-native) overrides divert to `invoke`.
        let generator_resumption =
            recv_value.is_generator() && matches!(name, "next" | "return" | "throw");
        if (recv_value.is_iterator() || recv_value.is_generator())
            && iterator_dispatch_method_name(name)
        {
            let method = self
                .get_method_value_for_call(context, stack, recv_value, name)?
                .unwrap_or_else(Value::undefined);
            let route_to_invoke = if generator_resumption {
                method.as_native_function().is_none() && self.is_callable_runtime(&method)
            } else {
                self.is_callable_runtime(&method)
            };
            if route_to_invoke {
                stack[top_idx].advance_pc(self.current_byte_len)?;
                return self.invoke(stack, context, &method, recv_value, arg_values, dst);
            }
            if recv_value.is_iterator() {
                return Err(VmError::NotCallable);
            }
            // Generator natives fall through to the resume block,
            // which threads the resumption argument and the async
            // promise machinery.
        }

        // §27.5.3 Generator.prototype methods — `.next` / `.return`
        // / `.throw`. The receiver carries the suspended frame; the
        // resume helper drives a sub-dispatch until the next Yield
        // or completion.
        // <https://tc39.es/ecma262/#sec-generator-objects>
        if let Some(g) = recv_value.as_generator() {
            let kind = match name {
                "next" => Some(GeneratorResumeKind::Next(
                    arg_values.first().cloned().unwrap_or(Value::undefined()),
                )),
                "return" => Some(GeneratorResumeKind::Return(
                    arg_values.first().cloned().unwrap_or(Value::undefined()),
                )),
                "throw" => Some(GeneratorResumeKind::Throw(
                    arg_values.first().cloned().unwrap_or(Value::undefined()),
                )),
                _ => None,
            };
            if let Some(kind) = kind {
                let is_async_gen = g.is_async(&self.gc_heap);
                if is_async_gen {
                    // §27.6.3 — async-generator method calls always
                    // return a Promise. Queue the request; only a
                    // suspended generator resumes immediately.
                    let cap = promise_dispatch::PromiseBuilder::with_context(context.clone())
                        .capability_stack_rooted(
                            self,
                            stack,
                            &[&recv_value],
                            &[arg_values.as_slice()],
                        )?;
                    let promise = cap.promise;

                    if g.async_state(&self.gc_heap)
                        == crate::generator::AsyncGeneratorState::Completed
                    {
                        match kind {
                            GeneratorResumeKind::Throw(reason) => {
                                self.async_generator_settle_capability(
                                    context,
                                    &cap,
                                    Err(reason),
                                    true,
                                )?;
                            }
                            GeneratorResumeKind::Next(_) => {
                                self.async_generator_settle_capability(
                                    context,
                                    &cap,
                                    Ok(Value::undefined()),
                                    true,
                                )?;
                            }
                            GeneratorResumeKind::Return(value) => {
                                self.async_generator_settle_capability(
                                    context,
                                    &cap,
                                    Ok(value),
                                    true,
                                )?;
                            }
                        }
                    } else {
                        let state = g.async_state(&self.gc_heap);
                        // §27.6.3.2 AsyncGeneratorResumeNext — a throw
                        // completion delivered while the body is still
                        // suspended-start closes the generator without
                        // ever resuming it; the request settles as a
                        // rejection.
                        if matches!(state, crate::generator::AsyncGeneratorState::SuspendedStart)
                            && let GeneratorResumeKind::Throw(reason) = kind
                        {
                            g.mark_done(&mut self.gc_heap);
                            g.set_async_state(
                                &mut self.gc_heap,
                                crate::generator::AsyncGeneratorState::Completed,
                            );
                            self.async_generator_settle_capability(
                                context,
                                &cap,
                                Err(reason),
                                true,
                            )?;
                        } else {
                            g.enqueue_async_request(&mut self.gc_heap, kind, cap.clone());
                            if matches!(
                                state,
                                crate::generator::AsyncGeneratorState::SuspendedStart
                                    | crate::generator::AsyncGeneratorState::SuspendedYield
                            ) {
                                let resume = g
                                    .front_async_resume(&self.gc_heap)
                                    .ok_or(VmError::InvalidOperand)?;
                                self.resume_generator(context, &g, resume)?;
                            }
                        }
                    }
                    let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                    write_register(frame, dst, promise)?;
                    frame.advance_pc(caller_byte_len)?;
                    return Ok(());
                }
                match self.resume_generator(context, &g, kind) {
                    Ok(result) => {
                        let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
                        write_register(frame, dst, result)?;
                        frame.advance_pc(self.current_byte_len)?;
                        return Ok(());
                    }
                    Err(err) => {
                        // If the generator body unwound an
                        // uncaught throw, re-raise the *original*
                        // value on the caller's frame stack so a
                        // surrounding `try { gen.throw(x) } catch`
                        // observes the right payload.
                        if let Some(thrown) = self.pending_generator_throw.take() {
                            self.unwind_throw(context, stack, thrown)?;
                            return Ok(());
                        }
                        return Err(err);
                    }
                }
            }
        }

        // §27.1.2 — Generator receivers walk through
        // `Iterator.prototype` for the iterator-helpers proposal
        // surface (`map` / `filter` / `take` / `drop` / `flatMap` /
        // `toArray` / `forEach` / `reduce` / `some` / `every` /
        // `find`). The direct Generator-method branch above only
        // handles `next` / `return` / `throw`; everything else
        // resolves through the global Iterator constructor's
        // prototype slot. Found methods invoke with the Generator
        // as the receiver so the foundation's
        // `iterator_receiver` wraps it on entry.
        if recv_value.is_generator() {
            let iterator_proto = {
                let v = crate::object::get(self.global_this, &self.gc_heap, "Iterator");
                if let Some(ctor) = v.and_then(|v| v.as_object()) {
                    crate::object::get(ctor, &self.gc_heap, "prototype")
                } else if let Some(ctor) = v.and_then(|v| v.as_native_function()) {
                    ctor.own_property_descriptor(&mut self.gc_heap, "prototype")
                        .ok()
                        .flatten()
                        .and_then(|d| match d.kind {
                            crate::object::DescriptorKind::Data { value } => Some(value),
                            _ => None,
                        })
                } else {
                    None
                }
            };
            if let Some(proto) = iterator_proto.and_then(|v| v.as_object())
                && let Some(method) = crate::object::get(proto, &self.gc_heap, name)
                && self.is_callable_runtime(&method)
            {
                stack[top_idx].advance_pc(self.current_byte_len)?;
                self.invoke(stack, context, &method, recv_value, arg_values, dst)?;
                return Ok(());
            }
        }

        // §7.3.11 GetMethod + §7.3.14 Call.
        if recv_value.is_array() {
            let method = self
                .get_method_value_for_call(context, stack, recv_value, name)?
                .unwrap_or_else(Value::undefined);
            if !self.is_callable_runtime(&method) {
                return Err(VmError::NotCallable);
            }
            stack[top_idx].advance_pc(self.current_byte_len)?;
            return self.invoke(stack, context, &method, recv_value, arg_values, dst);
        }
        // §7.3.11 GetMethod + §7.3.14 Call.
        if name == "charCodeAt"
            && recv_value.is_string()
            && let Some(result) =
                self.try_fast_primitive_string_char_code_at(recv_value, arg_values.as_slice())?
        {
            stack[top_idx].advance_pc(self.current_byte_len)?;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(());
        }
        // §7.3.11 GetMethod + §7.3.14 Call.
        if name == "toString"
            && recv_value.is_number()
            && let Some(result) =
                self.try_fast_primitive_number_to_string(recv_value, arg_values.as_slice())?
        {
            stack[top_idx].advance_pc(self.current_byte_len)?;
            write_register(&mut stack[top_idx], dst, result)?;
            return Ok(());
        }
        // §7.3.11 GetMethod + §7.3.14 Call.
        if self.has_plain_builtin_method(recv_value, name) {
            // Primitive-method inline cache. A builtin method on a primitive
            // string / number is an own-data slot of `%String.prototype%` /
            // `%Number.prototype%`; the primitive itself owns no property of
            // that name (`has_plain_builtin_method` established it is a builtin
            // method name, not `length` / an index). Resolving through the
            // shape-guarded own-data IC on the well-known prototype skips the
            // per-call ToObject → constructor → prototype walk and the named
            // `[[Get]]` lookup. A method addition changes the prototype shape
            // (guard miss → re-resolve); an in-place override is read live from
            // the slot — both stay correct.
            if let Some(proto) = self.primitive_method_proto(recv_value)
                && let Some(atomized_key) =
                    context.property_atom_for_function(stack[top_idx].function_id, name_idx)
                && let Some(site) =
                    context.property_ic_site(stack[top_idx].function_id, stack[top_idx].pc)
                && let Some(method) = self.resolve_method_ic(proto, atomized_key, site)
                && self.is_callable_runtime(&method)
            {
                stack[top_idx].advance_pc(self.current_byte_len)?;
                return self.invoke(stack, context, &method, recv_value, arg_values, dst);
            }
            let method = self
                .get_method_value_for_call(context, stack, recv_value, name)?
                .unwrap_or_else(Value::undefined);
            if !self.is_callable_runtime(&method) {
                return Err(VmError::NotCallable);
            }
            stack[top_idx].advance_pc(self.current_byte_len)?;
            return self.invoke(stack, context, &method, recv_value, arg_values, dst);
        }
        // §9.4.5 integer-indexed exotic: an own expando property shadows
        // any inherited prototype method.
        if let Some(method) =
            self.typed_array_own_method_value_for_call(context, recv_value, name)?
        {
            if !self.is_callable_runtime(&method) {
                return Err(VmError::NotCallable);
            }
            stack[top_idx].advance_pc(self.current_byte_len)?;
            return self.invoke(stack, context, &method, recv_value, arg_values, dst);
        }
        // §7.3.11 GetMethod + §7.3.14 Call.
        if recv_value.is_typed_array() {
            let method = self
                .get_method_value_for_call(context, stack, recv_value, name)?
                .unwrap_or_else(Value::undefined);
            if !self.is_callable_runtime(&method) {
                return Err(VmError::NotCallable);
            }
            stack[top_idx].advance_pc(self.current_byte_len)?;
            return self.invoke(stack, context, &method, recv_value, arg_values, dst);
        }
        // §22.1.3.18 / §22.1.3.19 — `String.prototype.replace` and
        // `replaceAll` with a callable replaceValue dispatch through
        // the interpreter to invoke the callback. The intrinsic
        // table can't run callbacks (it lacks an
        // `ExecutionContext`), so intercept here before the table
        // lookup and route through the dedicated bridge.
        //
        // Wrapper objects (`new String("…")`) also reach this arm —
        // unwrap their `[[StringData]]` so the receiver flows in as
        // a primitive string for the callable-replace bridge.
        let string_recv: Option<Value> = if recv_value.is_string() {
            Some(recv_value)
        } else if let Some(obj) = recv_value.as_object() {
            crate::object::string_data(obj, &self.gc_heap).map(Value::string)
        } else {
            None
        };
        if let Some(string_recv) = string_recv
            && (name == "replace" || name == "replaceAll")
            && arg_values.len() >= 2
            && self.is_callable_runtime(&arg_values[1])
            && !arg_values.first().is_some_and(|v| v.is_regexp())
        {
            if recv_value.as_object().is_some() {
                let method = self
                    .get_method_value_for_call(context, stack, recv_value, name)?
                    .unwrap_or_else(Value::undefined);
                if method.as_native_function().is_none() {
                    if !self.is_callable_runtime(&method) {
                        return Err(VmError::NotCallable);
                    }
                    stack[top_idx].advance_pc(self.current_byte_len)?;
                    return self.invoke(stack, context, &method, recv_value, arg_values, dst);
                }
            }
            let recv_value = string_recv;
            // §22.1.3.18 step 7 — `searchString = ? ToString(searchValue)`.
            // Coerce non-String searchValues (null, undefined, numbers,
            // objects with `toString`) before handing the args to the
            // callable-replace bridge.
            let mut coerced_args = arg_values.clone();
            let needs_coerce = !coerced_args.first().is_some_and(|v| v.is_string());
            if needs_coerce {
                let original = coerced_args.first().cloned().unwrap_or(Value::undefined());
                let coerced = if original.is_undefined() {
                    "undefined".to_string()
                } else if original.is_null() {
                    "null".to_string()
                } else if let Some(b) = original.as_boolean() {
                    if b { "true" } else { "false" }.to_string()
                } else if let Some(n) = original.as_number() {
                    n.to_display_string()
                } else if let Some(b) = original.as_big_int() {
                    b.to_decimal_string(&self.gc_heap)
                } else if original.is_symbol() {
                    return Err(self.err_type(
                        ("Cannot convert a Symbol value to a string".to_string()).into(),
                    ));
                } else if original.is_object()
                    || original.is_array()
                    || original.is_function()
                    || original.is_closure()
                    || original.is_native_function()
                    || original.is_bound_function()
                    || original.is_class_constructor()
                    || original.is_proxy()
                {
                    let primitive = self.evaluate_to_primitive(
                        context,
                        &original,
                        crate::abstract_ops::ToPrimitiveHint::String,
                    )?;
                    if let Some(s) = primitive.as_string(&self.gc_heap) {
                        s.to_lossy_string(&self.gc_heap)
                    } else if let Some(n) = primitive.as_number() {
                        n.to_display_string()
                    } else if let Some(b) = primitive.as_boolean() {
                        if b { "true" } else { "false" }.to_string()
                    } else if primitive.is_null() {
                        "null".to_string()
                    } else if primitive.is_undefined() {
                        "undefined".to_string()
                    } else if let Some(b) = primitive.as_big_int() {
                        b.to_decimal_string(&self.gc_heap)
                    } else if primitive.is_symbol() {
                        return Err(self.err_type(
                            ("Cannot convert a Symbol value to a string".to_string()).into(),
                        ));
                    } else {
                        return Err(VmError::TypeMismatch);
                    }
                } else {
                    return Err(VmError::TypeMismatch);
                };
                if let Some(slot) = coerced_args.first_mut() {
                    *slot = Value::string(JsString::from_str(&coerced, self.gc_heap_mut())?);
                }
            }
            stack[top_idx].advance_pc(self.current_byte_len)?;
            let result = self.dispatch_string_callable_replace(
                context,
                &recv_value,
                &coerced_args,
                name == "replaceAll",
            )?;
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            return Ok(());
        }
        if recv_value.as_function().is_some() || recv_value.as_closure(&self.gc_heap).is_some() {
            let is_function_intrinsic = function_prototype_intrinsic_name(name);
            if is_function_intrinsic || object_prototype_dispatch_method_name(name) {
                let method = self
                    .get_method_value_for_call(context, stack, recv_value, name)?
                    .unwrap_or_else(Value::undefined);
                if !self.is_callable_runtime(&method) {
                    return Err(VmError::NotCallable);
                }
                if is_function_intrinsic
                    && is_function_prototype_intrinsic_value(
                        method,
                        &self.gc_heap,
                        function_prototype_intrinsic(name),
                    )
                {
                    return self.dispatch_function_method(
                        stack,
                        context,
                        &recv_value,
                        name,
                        arg_values,
                        dst,
                    );
                }
                stack[top_idx].advance_pc(caller_byte_len)?;
                return self.invoke(stack, context, &method, recv_value, arg_values, dst);
            }
        }
        // Functions / closures inherit Object.prototype-style
        // methods. Foundation routes the call through the user-
        // properties bag attached to the compiled function.
        let fn_id_for_proto = recv_value.as_function().or_else(|| {
            recv_value
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        });
        if fn_id_for_proto.is_some() && object_prototype_dispatch_method_name(name) {
            let method = self
                .get_method_value_for_call(context, stack, recv_value, name)?
                .unwrap_or_else(Value::undefined);
            if !self.is_callable_runtime(&method) {
                return Err(VmError::NotCallable);
            }
            stack[top_idx].advance_pc(self.current_byte_len)?;
            return self.invoke(stack, context, &method, recv_value, arg_values, dst);
        }
        if recv_value.as_native_function().is_some() && object_prototype_dispatch_method_name(name)
        {
            let method = self.ordinary_method_value_for_call(context, recv_value, name)?;
            if !self.is_callable_runtime(&method) {
                return Err(VmError::NotCallable);
            }
            stack[top_idx].advance_pc(self.current_byte_len)?;
            return self.invoke(stack, context, &method, recv_value, arg_values, dst);
        }
        if recv_value.as_bound_function().is_some() && object_prototype_dispatch_method_name(name) {
            let method = self.ordinary_method_value_for_call(context, recv_value, name)?;
            if !self.is_callable_runtime(&method) {
                return Err(VmError::NotCallable);
            }
            stack[top_idx].advance_pc(self.current_byte_len)?;
            return self.invoke(stack, context, &method, recv_value, arg_values, dst);
        }
        // §7.1.18 ToObject — `String.prototype.hasOwnProperty(idx)`,
        // `(0).propertyIsEnumerable("toString")`, etc. inherit
        // `Object.prototype.{hasOwnProperty, propertyIsEnumerable,
        // isPrototypeOf}` through the primitive wrapper chain. The
        // wrapper isn't materialized; we answer directly from the
        // primitive shape: String exposes integer indices in
        // `[0, length)` plus `"length"`; every other primitive has
        // no own properties.
        if object_prototype_dispatch_method_name(name)
            && (recv_value.is_string()
                || recv_value.is_number()
                || recv_value.is_boolean()
                || recv_value.is_symbol()
                || recv_value.is_big_int())
        {
            let method = self
                .get_method_value_for_call(context, stack, recv_value, name)?
                .unwrap_or_else(Value::undefined);
            if !self.is_callable_runtime(&method) {
                return Err(VmError::NotCallable);
            }
            stack[top_idx].advance_pc(self.current_byte_len)?;
            return self.invoke(stack, context, &method, recv_value, arg_values, dst);
        }

        if self.is_callable_runtime(&recv_value)
            && !recv_value.is_proxy()
            && function_prototype_intrinsic_name(name)
            && self.callable_has_own_function_method_shadow(context, recv_value, name)?
        {
            let method = self
                .get_method_value_for_call(context, stack, recv_value, name)?
                .unwrap_or_else(Value::undefined);
            if !self.is_callable_runtime(&method) {
                return Err(VmError::NotCallable);
            }
            stack[top_idx].advance_pc(self.current_byte_len)?;
            return self.invoke(stack, context, &method, recv_value, arg_values, dst);
        }

        if self.is_callable_runtime(&recv_value)
            && !recv_value.is_proxy()
            && function_prototype_intrinsic_name(name)
        {
            return self.dispatch_function_method(
                stack,
                context,
                &recv_value,
                name,
                arg_values,
                dst,
            );
        }

        if let Some(method) = self.get_method_value_for_call(context, stack, recv_value, name)? {
            if !self.is_callable_runtime(&method) {
                return Err(VmError::NotCallable);
            }
            if self.is_callable_runtime(&recv_value)
                && function_prototype_intrinsic_name(name)
                && is_function_prototype_intrinsic_value(
                    method,
                    &self.gc_heap,
                    function_prototype_intrinsic(name),
                )
            {
                return self.dispatch_function_method(
                    stack,
                    context,
                    &recv_value,
                    name,
                    arg_values,
                    dst,
                );
            }
            stack[top_idx].advance_pc(self.current_byte_len)?;
            return self.invoke(stack, context, &method, recv_value, arg_values, dst);
        }

        // `Function.prototype.{call, apply, bind, toString}` on a
        // callable receiver that doesn't expose the method as a
        // property — fallback path.
        if matches!(name, "call" | "apply" | "bind" | "toString")
            && self.is_callable_runtime(&recv_value)
        {
            return self.dispatch_function_method(
                stack,
                context,
                &recv_value,
                name,
                arg_values,
                dst,
            );
        }

        Err(self.err_unknown_intrinsic(name.to_string().into()))
    }

    /// `true` when `recv_value`'s prototype defines a builtin method
    /// `name` that dispatches through §7.3.11 `GetMethod` + §7.3.14 `Call`
    /// without call-site coercion or a species step (the native handles
    /// those itself).
    fn has_plain_builtin_method(&self, recv_value: Value, name: &str) -> bool {
        if recv_value.is_string() {
            return string_prototype::is_builtin_method(name);
        }
        if recv_value.is_number() {
            return number::prototype::is_builtin_method(name);
        }
        if recv_value.is_boolean() {
            return boolean_prototype::is_builtin_method(name);
        }
        if recv_value.is_big_int() {
            return bigint::prototype::is_builtin_method(name);
        }
        if recv_value.is_symbol() {
            return symbol_prototype::is_builtin_method(name);
        }
        if recv_value.is_regexp() {
            return regexp_prototype::is_builtin_method(name);
        }
        if recv_value.is_map() {
            return collections_prototype::is_map_builtin_method(name);
        }
        if recv_value.is_set() {
            return collections_prototype::is_set_builtin_method(name);
        }
        if recv_value.is_weak_map() {
            return collections_prototype::is_weak_map_builtin_method(name);
        }
        if recv_value.is_weak_set() {
            return collections_prototype::is_weak_set_builtin_method(name);
        }
        if recv_value.is_weak_ref() {
            return weak_refs::is_weak_ref_builtin_method(name);
        }
        if recv_value.is_finalization_registry() {
            return weak_refs::is_finalization_registry_builtin_method(name);
        }
        if recv_value
            .as_object()
            .is_some_and(|o| crate::object::date_data(o, &self.gc_heap).is_some())
        {
            return date::prototype::is_builtin_method(name);
        }
        false
    }

    /// JIT bridge for `CallMethodValue` (`recv.name(args…)`) from compiled code.
    ///
    /// Resolves the method through the full `[[Get]]` ladder
    /// ([`Self::get_method_value_for_call`]) and invokes it synchronously with
    /// `this` = `recv` via [`Self::run_callable_sync`] — the same primitive the
    /// `Op::Call` bridge uses, so native and ordinary bytecode methods complete
    /// inline and the result lands in `dst`. The frame PC is saved/restored so a
    /// later guard bail re-runs the compiled frame from PC 0.
    ///
    /// # Errors
    /// `TypeError` for a nullish receiver, `NotCallable` when the resolved
    /// property is not callable, plus any error the method itself throws.
    pub fn jit_runtime_call_method(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        frame_index: usize,
        dst: u16,
        recv_reg: u16,
        name_idx: u32,
        call_byte_pc: u32,
        site: usize,
        arg_regs: &[u16],
    ) -> Result<(), VmError> {
        self.record_jit_runtime_method_stub();
        let recv = *read_register(&stack[frame_index], recv_reg)?;
        if recv.is_nullish() {
            let label = if recv.is_null() { "null" } else { "undefined" };
            return Err(self.err_type((format!("Cannot read properties of {label}")).into()));
        }
        let mut args: SmallVec<[Value; 8]> = SmallVec::with_capacity(arg_regs.len());
        for &r in arg_regs {
            args.push(*read_register(&stack[frame_index], r)?);
        }
        // Cached dense-array builtin: a guard hit dispatches without resolving
        // the method name, hashing the prototype slot, or string-matching.
        if let Some(result) = self.try_array_method_call_ic(context, site, recv, args.as_slice()) {
            let value = result?;
            write_register(&mut stack[frame_index], dst, value)?;
            return Ok(());
        }
        if let Some(result) = self.try_collection_method_call_ic(site, recv, args.as_slice()) {
            let value = result?;
            write_register(&mut stack[frame_index], dst, value)?;
            return Ok(());
        }
        let name = context
            .string_constant_str_for_function(stack[frame_index].function_id, name_idx)
            .ok_or(VmError::InvalidOperand)?;
        if let Some(result) =
            self.try_fast_array_proto_method(context, site, recv, name, args.as_slice())
        {
            let value = result?;
            write_register(&mut stack[frame_index], dst, value)?;
            return Ok(());
        }
        if let Some(result) =
            self.try_fast_collection_proto_method(site, recv, name, args.as_slice())
        {
            let value = result?;
            write_register(&mut stack[frame_index], dst, value)?;
            return Ok(());
        }
        if name == "charCodeAt"
            && recv.is_string()
            && let Some(result) =
                self.try_fast_primitive_string_char_code_at(recv, args.as_slice())?
        {
            write_register(&mut stack[frame_index], dst, result)?;
            return Ok(());
        }
        if name == "toString"
            && recv.is_number()
            && let Some(result) = self.try_fast_primitive_number_to_string(recv, args.as_slice())?
        {
            write_register(&mut stack[frame_index], dst, result)?;
            return Ok(());
        }
        let saved_pc = stack[frame_index].pc;
        let method = self
            .get_method_value_for_call(context, stack, recv, name)?
            .unwrap_or_else(Value::undefined);
        if !self.is_callable_runtime(&method) {
            stack[frame_index].pc = saved_pc;
            return Err(VmError::NotCallable);
        }
        if self.jit_hook.is_some()
            && let Ok((method_fid, _, _, _, _, _)) =
                Self::bytecode_call_target_parts(method, recv, &self.gc_heap)
        {
            let caller_fid = stack[frame_index].function_id;
            if let Some(site) = self.method_site_for_receiver(context, caller_fid, name_idx, recv) {
                self.note_method_target(caller_fid, call_byte_pc, method_fid, site);
            }
        }
        let result = self.run_callable_sync(context, &method, recv, args)?;
        stack[frame_index].pc = saved_pc;
        write_register(&mut stack[frame_index], dst, result)?;
        Ok(())
    }

    /// JIT bridge for the leaf/no-allocation Map/Set method path.
    ///
    /// Returns `Ok(true)` when a live collection method IC validates and the
    /// matching leaf runtime stub produced a value written to `dst`.
    /// `Ok(false)` is a guard/stub miss; compiled code must continue to the
    /// existing direct-call/full-method fallback path. This bridge never
    /// performs method resolution or calls user code.
    pub fn jit_runtime_try_collection_leaf_method(
        &mut self,
        stack: &mut HoltStack,
        frame_index: usize,
        dst: u16,
        recv_reg: u16,
        site: usize,
        arg_regs: &[u16],
    ) -> Result<bool, VmError> {
        let Some(stub_id) = self.jit_runtime_resolve_collection_leaf_method_stub(
            stack,
            frame_index,
            recv_reg,
            site,
        )?
        else {
            return Ok(false);
        };
        let recv = *read_register(&stack[frame_index], recv_reg)?;
        let key = if let Some(&reg) = arg_regs.first() {
            *read_register(&stack[frame_index], reg)?
        } else {
            Value::undefined()
        };
        let result = crate::runtime_stubs::leaf_no_alloc_stub2_trampoline(
            &self.gc_heap as *const otter_gc::GcHeap,
            stub_id,
            recv.to_abi_bits(),
            key.to_abi_bits(),
        );
        let Some(value) = result.into_value() else {
            return Ok(false);
        };
        write_register(&mut stack[frame_index], dst, value)?;
        Ok(true)
    }

    /// JIT bridge for the guarded collection method IC only.
    ///
    /// Returns the leaf stub descriptor id when receiver/prototype/builtin
    /// guards validate. The caller is responsible for invoking the returned
    /// VM-native leaf ABI entry against raw register-window values.
    pub fn jit_runtime_resolve_collection_leaf_method_stub(
        &mut self,
        stack: &HoltStack,
        frame_index: usize,
        recv_reg: u16,
        site: usize,
    ) -> Result<Option<RuntimeStubId>, VmError> {
        let recv = *read_register(&stack[frame_index], recv_reg)?;
        if recv.is_nullish() {
            return Ok(None);
        }
        Ok(self
            .collection_method_call_ic_target(site, recv)
            .and_then(|target| target.leaf_stub_id))
    }

    /// Snapshot a collection leaf method IC into JIT-readable guard metadata.
    ///
    /// This is intentionally stricter than the runtime IC guard: explicit
    /// prototype overrides, even if they point back to the canonical prototype,
    /// are left to the normal fallback path because generated code only checks
    /// the collection body's no-override/no-expando guard flags.
    pub(crate) fn jit_collection_leaf_method_feedback(
        &self,
        site: usize,
    ) -> Option<crate::jit::JitCollectionLeafMethod> {
        let ic = match (*self.method_call_ics.get(site)?).as_ref().copied()? {
            MethodCallIc::Collection(ic) => ic,
            MethodCallIc::Array(_) => return None,
        };
        let stub_id = ic.leaf_stub_id?;
        let (proto, receiver_type_tag) = if ic.op.is_map() {
            (
                self.realm_intrinsics.map_prototype?,
                crate::collections::MAP_BODY_TYPE_TAG,
            )
        } else {
            (
                self.realm_intrinsics.set_prototype?,
                crate::collections::SET_BODY_TYPE_TAG,
            )
        };
        if crate::object::shape_id(proto, &self.gc_heap) != ic.proto_shape {
            return None;
        }
        let method = crate::object::data_slot_value_at(proto, &self.gc_heap, ic.proto_slot)?;
        if !ic.op.matches_builtin(method, &self.gc_heap) {
            return None;
        }
        let builtin_fn_addr = method
            .as_native_function()
            .and_then(|native| native.jit_static_fn_addr(&self.gc_heap))?;
        Some(crate::jit::JitCollectionLeafMethod {
            receiver_type_tag,
            proto_offset: proto.offset(),
            proto_shape: crate::object::shape(proto, &self.gc_heap).offset(),
            method_value_byte: u32::from(ic.proto_slot) * std::mem::size_of::<Value>() as u32,
            builtin_fn_addr,
            leaf_stub_id: stub_id,
        })
    }

    /// Snapshot a collection allocating method IC into JIT-readable guard
    /// metadata.
    ///
    /// This deliberately carries no safepoint id. The backend owns
    /// instruction-level safepoint creation and must only call the descriptor's
    /// allocating ABI entry after publishing a precise root map for receiver,
    /// arguments, live frame slots, and tagged machine values.
    pub(crate) fn jit_collection_alloc_method_feedback(
        &self,
        site: usize,
        safepoint_id: crate::native_abi::SafepointId,
    ) -> Option<crate::jit::JitCollectionAllocMethod> {
        let ic = match (*self.method_call_ics.get(site)?).as_ref().copied()? {
            MethodCallIc::Collection(ic) => ic,
            MethodCallIc::Array(_) => return None,
        };
        let stub_id = ic.alloc_stub_id?;
        let (proto, receiver_type_tag) = if ic.op.is_map() {
            (
                self.realm_intrinsics.map_prototype?,
                crate::collections::MAP_BODY_TYPE_TAG,
            )
        } else {
            (
                self.realm_intrinsics.set_prototype?,
                crate::collections::SET_BODY_TYPE_TAG,
            )
        };
        if crate::object::shape_id(proto, &self.gc_heap) != ic.proto_shape {
            return None;
        }
        let method = crate::object::data_slot_value_at(proto, &self.gc_heap, ic.proto_slot)?;
        if !ic.op.matches_builtin(method, &self.gc_heap) {
            return None;
        }
        let builtin_fn_addr = method
            .as_native_function()
            .and_then(|native| native.jit_static_fn_addr(&self.gc_heap))?;
        Some(crate::jit::JitCollectionAllocMethod {
            receiver_type_tag,
            proto_offset: proto.offset(),
            proto_shape: crate::object::shape(proto, &self.gc_heap).offset(),
            method_value_byte: u32::from(ic.proto_slot) * std::mem::size_of::<Value>() as u32,
            builtin_fn_addr,
            alloc_stub_id: stub_id,
            safepoint_id,
            value_arg_count: 3,
        })
    }

    /// Fast `arr.method(args)` dispatch for an ordinary dense array whose
    /// `%Array.prototype%` slot for `method` is still the original builtin,
    /// skipping the per-call `[[Get]]` method-resolution walk and the native
    /// call bridge entirely.
    ///
    /// Returns `None` (caller falls back to the full path) unless every
    /// condition that makes the direct dispatch observably identical holds:
    /// the receiver is an ordinary dense array (no exotic sidecar, so no own
    /// property can shadow the inherited method and its `[[Prototype]]` is the
    /// realm `%Array.prototype%`); the prototype's own slot for `method` is a
    /// native function whose name matches `method` (any user override installs
    /// a different function — a closure fails `as_native_function`, a different
    /// builtin fails the name check — and a `delete` removes the own slot); and
    /// [`Self::array_live_method_dispatch`] actually handles the name. When all
    /// hold, the resolved value can only be the canonical builtin, so calling
    /// the live dispatcher directly preserves spec semantics.
    ///
    /// The receiver and arguments are rooted across the dispatch because an
    /// array method can scavenge and they live only on this native stack frame.
    fn try_fast_array_proto_method(
        &mut self,
        context: &ExecutionContext,
        site: usize,
        recv: Value,
        name: &str,
        args: &[Value],
    ) -> Option<Result<Value, VmError>> {
        let arr = recv.as_array()?;
        if !crate::array::is_ordinary_dense(arr, &self.gc_heap) {
            return None;
        }
        let proto = self.realm_intrinsics.array_prototype?;
        let (hit, lookup) = crate::object::lookup_own_slot(proto, &self.gc_heap, name);
        let method = match lookup {
            crate::object::PropertyLookup::Data { value, .. } => value,
            _ => return None,
        };
        if !crate::array_prototype::is_array_prototype_builtin(method, &self.gc_heap, name) {
            return None;
        }
        let tag = crate::array_prototype::ArrayMethodTag::from_name(name)?;
        // Install the call-site IC so subsequent calls skip name resolution and
        // the slot hash. The cached slot offset is only sound while the
        // prototype keeps the recorded shape (guarded on the fast path).
        if let Some(hit) = hit
            && site < self.method_call_ics.len()
        {
            self.method_call_ics[site] = Some(MethodCallIc::Array(ArrayMethodCallIc {
                proto_shape: hit.shape_id,
                proto_slot: hit.slot,
                tag,
            }));
        }
        Some(self.dispatch_array_builtin_rooted(context, tag, recv, args))
    }

    /// Fast `arr.method(args)` dispatch through the call-site method IC.
    ///
    /// Returns `None` (caller falls back to the full resolution, which may
    /// re-install the IC) unless the receiver is still an ordinary dense array
    /// and the realm `%Array.prototype%` still carries the recorded builtin at
    /// the cached shape + slot. On a hit the resolved value can only be the
    /// canonical builtin, so dispatching the cached tag directly preserves spec
    /// semantics — identical to [`Self::try_fast_array_proto_method`] but
    /// without the per-call name lookup.
    fn try_array_method_call_ic(
        &mut self,
        context: &ExecutionContext,
        site: usize,
        recv: Value,
        args: &[Value],
    ) -> Option<Result<Value, VmError>> {
        let ic = match (*self.method_call_ics.get(site)?).as_ref().copied()? {
            MethodCallIc::Array(ic) => ic,
            MethodCallIc::Collection(_) => return None,
        };
        let Some(arr) = recv.as_array() else {
            // The receiver is no longer an array: drop the cache so the direct
            // compiled-call path (skipped while the IC was live) resumes for
            // whatever this site now sees.
            self.method_call_ics[site] = None;
            return None;
        };
        if !crate::array::is_ordinary_dense(arr, &self.gc_heap) {
            return None;
        }
        let proto = self.realm_intrinsics.array_prototype?;
        if crate::object::shape_id(proto, &self.gc_heap) != ic.proto_shape {
            return None;
        }
        let method = crate::object::data_slot_value_at(proto, &self.gc_heap, ic.proto_slot)?;
        if !ic.tag.matches_builtin(method, &self.gc_heap) {
            return None;
        }
        Some(self.dispatch_array_builtin_rooted(context, ic.tag, recv, args))
    }

    /// Fast Map/Set builtin dispatch through the call-site method IC.
    ///
    /// The hit path avoids the method-name constant fetch, prototype slot hash,
    /// and operation string-match. It still validates all observable guards:
    /// receiver family, canonical prototype/no expando, prototype shape, and
    /// builtin identity at the cached slot.
    fn try_collection_method_call_ic(
        &mut self,
        site: usize,
        recv: Value,
        args: &[Value],
    ) -> Option<Result<Value, VmError>> {
        let target = self.collection_method_call_ic_target(site, recv)?;
        Some(self.dispatch_collection_builtin(target, recv, args))
    }

    fn collection_method_call_ic_target(
        &mut self,
        site: usize,
        recv: Value,
    ) -> Option<CollectionFastTarget> {
        let ic = match (*self.method_call_ics.get(site)?).as_ref().copied()? {
            MethodCallIc::Collection(ic) => ic,
            MethodCallIc::Array(_) => return None,
        };
        let proto = if let Some(map) = recv.as_map() {
            if !ic.op.is_map() {
                self.method_call_ics[site] = None;
                return None;
            }
            let proto = self.realm_intrinsics.map_prototype?;
            if !prototype_override_is(
                crate::collections::map_prototype_override(map, &self.gc_heap),
                proto,
            ) || crate::collections::map_expando(map, &self.gc_heap).is_some()
            {
                return None;
            }
            proto
        } else if let Some(set) = recv.as_set() {
            if !ic.op.is_set() {
                self.method_call_ics[site] = None;
                return None;
            }
            let proto = self.realm_intrinsics.set_prototype?;
            if !prototype_override_is(
                crate::collections::set_prototype_override(set, &self.gc_heap),
                proto,
            ) || crate::collections::set_expando(set, &self.gc_heap).is_some()
            {
                return None;
            }
            proto
        } else {
            self.method_call_ics[site] = None;
            return None;
        };
        if crate::object::shape_id(proto, &self.gc_heap) != ic.proto_shape {
            return None;
        }
        let method = crate::object::data_slot_value_at(proto, &self.gc_heap, ic.proto_slot)?;
        if !ic.op.matches_builtin(method, &self.gc_heap) {
            return None;
        }
        Some(CollectionFastTarget {
            op: ic.op,
            leaf_stub_id: ic.leaf_stub_id,
        })
    }

    /// Direct `map.get/set/has/delete` and `set.add/has/delete` dispatch on an
    /// ordinary Map/Set whose realm prototype slot still holds the original
    /// builtin — skipping the per-call method-resolution walk
    /// (`get_method_value_for_call` → `ordinary_get_value` → constructor →
    /// prototype) **and** the native call bridge (`invoke` →
    /// `invoke_native_call_with_roots` → `NativeCtx`) entirely, calling the
    /// `collections::*` primitive straight through.
    ///
    /// Returns `None` (caller falls back to the full path) unless every
    /// condition that makes the direct call observably identical holds: the
    /// receiver is an ordinary Map/Set with no per-instance prototype override
    /// and no own expando (either could shadow the inherited method), and the
    /// realm prototype's own slot for `name` is still the canonical builtin (a
    /// user override or deletion fails the function-pointer guard).
    fn try_fast_collection_proto_method(
        &mut self,
        site: usize,
        recv: Value,
        name: &str,
        args: &[Value],
    ) -> Option<Result<Value, VmError>> {
        use crate::object::PropertyLookup;
        let (hit, op) = if let Some(map) = recv.as_map() {
            let proto = self.realm_intrinsics.map_prototype?;
            // Accept the canonical prototype whether it is the implicit default
            // (`None`) or the explicit `[[Prototype]]` recorded at construction;
            // a user-installed prototype is a different object and bails.
            if !prototype_override_is(
                crate::collections::map_prototype_override(map, &self.gc_heap),
                proto,
            ) || crate::collections::map_expando(map, &self.gc_heap).is_some()
            {
                return None;
            }
            let (hit, lookup) = crate::object::lookup_own_slot(proto, &self.gc_heap, name);
            let PropertyLookup::Data { value: method, .. } = lookup else {
                return None;
            };
            if !crate::bootstrap_collections::is_map_prototype_builtin(method, &self.gc_heap, name)
            {
                return None;
            }
            let op = CollectionFastOp::from_map_name(name)?;
            (hit, op)
        } else if let Some(set) = recv.as_set() {
            let proto = self.realm_intrinsics.set_prototype?;
            if !prototype_override_is(
                crate::collections::set_prototype_override(set, &self.gc_heap),
                proto,
            ) || crate::collections::set_expando(set, &self.gc_heap).is_some()
            {
                return None;
            }
            let (hit, lookup) = crate::object::lookup_own_slot(proto, &self.gc_heap, name);
            let PropertyLookup::Data { value: method, .. } = lookup else {
                return None;
            };
            if !crate::bootstrap_collections::is_set_prototype_builtin(method, &self.gc_heap, name)
            {
                return None;
            }
            let op = CollectionFastOp::from_set_name(name)?;
            (hit, op)
        } else {
            return None;
        };
        if let Some(hit) = hit
            && site < self.method_call_ics.len()
        {
            self.method_call_ics[site] = Some(MethodCallIc::Collection(CollectionMethodCallIc {
                proto_shape: hit.shape_id,
                proto_slot: hit.slot,
                op,
                leaf_stub_id: op.leaf_stub_id(),
                alloc_stub_id: op.alloc_stub_id(),
            }));
        }
        Some(self.dispatch_collection_builtin(CollectionFastTarget::new(op), recv, args))
    }

    /// Run a resolved Map/Set builtin, taking a leaf no-allocation path for
    /// lookup operations whose key is already representable without
    /// flattening. All allocating or materialising cases fall back to the
    /// rooted path below.
    fn dispatch_collection_builtin(
        &mut self,
        target: CollectionFastTarget,
        recv: Value,
        args: &[Value],
    ) -> Result<Value, VmError> {
        if let Some(stub_id) = target.leaf_stub_id
            && let Some(value) = self
                .dispatch_collection_builtin_leaf_no_alloc(stub_id, recv, args)
                .into_value()
        {
            return Ok(value);
        }
        self.dispatch_collection_builtin_rooted(target.op, recv, args)
    }

    /// Leaf Map/Set lookup dispatch.
    ///
    /// This path must not allocate, trigger GC, flatten strings, call JS, or
    /// mutate collection state. If the key would need materialisation for
    /// efficient SameValueZero comparison, return `Miss` and let the rooted
    /// allocating path handle it.
    fn dispatch_collection_builtin_leaf_no_alloc(
        &self,
        stub_id: RuntimeStubId,
        recv: Value,
        args: &[Value],
    ) -> crate::native_abi::RuntimeStubResult {
        let key = args.first().copied().unwrap_or_else(Value::undefined);
        crate::runtime_stubs::invoke_leaf_no_alloc_stub2(&self.gc_heap, stub_id, recv, key)
    }

    /// Run the resolved Map/Set builtin with the receiver and arguments rooted
    /// (an inserting `set` / `add` can scavenge). Flattens a string key in place
    /// so the per-entry SameValueZero compare runs flat (see
    /// `equals_string_bodies`); for the inserting ops the forwarded receiver is
    /// read back from the root slot to return the relocated collection.
    fn dispatch_collection_builtin_rooted(
        &mut self,
        op: CollectionFastOp,
        recv: Value,
        args: &[Value],
    ) -> Result<Value, VmError> {
        let key = args.first().copied().unwrap_or_else(Value::undefined);
        let roots = CollectionFastDispatchRoots {
            interp_roots: otter_gc::ExtraRoots::new::<Interpreter>(self),
            recv,
            args,
        };
        let depth = self
            .gc_heap
            .push_extra_roots(otter_gc::ExtraRoots::new(&roots));
        // Flatten the string key once so stored keys stay flat and lookups
        // compare flat-vs-flat instead of re-materializing a rope per probe.
        if let Some(s) = key.as_string(&self.gc_heap) {
            let _ = s.flatten_in_place(&mut self.gc_heap);
        }
        let result = (|| -> Result<Value, VmError> {
            match op {
                CollectionFastOp::MapGet => {
                    let map = roots.recv.as_map().ok_or(VmError::InvalidOperand)?;
                    Ok(crate::collections::map_get(map, &self.gc_heap, &key)
                        .unwrap_or_else(Value::undefined))
                }
                CollectionFastOp::MapHas => {
                    let map = roots.recv.as_map().ok_or(VmError::InvalidOperand)?;
                    Ok(Value::boolean(crate::collections::map_has(
                        map,
                        &self.gc_heap,
                        &key,
                    )))
                }
                CollectionFastOp::MapDelete => {
                    let map = roots.recv.as_map().ok_or(VmError::InvalidOperand)?;
                    Ok(Value::boolean(crate::collections::map_delete(
                        map,
                        &mut self.gc_heap,
                        &key,
                    )))
                }
                CollectionFastOp::MapSet => {
                    let map = roots.recv.as_map().ok_or(VmError::InvalidOperand)?;
                    let value = args.get(1).copied().unwrap_or_else(Value::undefined);
                    crate::collections::map_set(map, &mut self.gc_heap, key, value)
                        .map_err(VmError::from)?;
                    Ok(roots.recv)
                }
                CollectionFastOp::SetHas => {
                    let set = roots.recv.as_set().ok_or(VmError::InvalidOperand)?;
                    Ok(Value::boolean(crate::collections::set_has(
                        set,
                        &self.gc_heap,
                        &key,
                    )))
                }
                CollectionFastOp::SetDelete => {
                    let set = roots.recv.as_set().ok_or(VmError::InvalidOperand)?;
                    Ok(Value::boolean(crate::collections::set_delete(
                        set,
                        &mut self.gc_heap,
                        &key,
                    )))
                }
                CollectionFastOp::SetAdd => {
                    let set = roots.recv.as_set().ok_or(VmError::InvalidOperand)?;
                    crate::collections::set_add(set, &mut self.gc_heap, key)
                        .map_err(VmError::from)?;
                    Ok(roots.recv)
                }
            }
        })();
        self.gc_heap.pop_extra_roots_to(depth - 1);
        result
    }

    /// Run a resolved `Array.prototype` builtin with the receiver and arguments
    /// rooted across the dispatch (an array method can scavenge and they live
    /// only on this native stack frame).
    fn dispatch_array_builtin_rooted(
        &mut self,
        context: &ExecutionContext,
        tag: crate::array_prototype::ArrayMethodTag,
        recv: Value,
        args: &[Value],
    ) -> Result<Value, VmError> {
        let roots = ArrayFastDispatchRoots {
            interp_roots: otter_gc::ExtraRoots::new::<Interpreter>(self),
            recv,
            args,
        };
        let depth = self
            .gc_heap
            .push_extra_roots(otter_gc::ExtraRoots::new(&roots));
        let result = self.array_live_method_dispatch(context, tag, recv, args, &[args]);
        self.gc_heap.pop_extra_roots_to(depth - 1);
        result
    }

    fn try_fast_primitive_string_char_code_at(
        &mut self,
        recv_value: Value,
        args: &[Value],
    ) -> Result<Option<Value>, VmError> {
        let Some(proto) = self.constructor_prototype_value("String")?.as_object() else {
            return Ok(None);
        };
        let value = match crate::object::lookup(proto, &self.gc_heap, "charCodeAt") {
            crate::object::PropertyLookup::Data { value, .. } => value,
            crate::object::PropertyLookup::Accessor { .. }
            | crate::object::PropertyLookup::Absent => {
                return Ok(None);
            }
        };
        if !string_prototype::is_char_code_at_builtin(value, &self.gc_heap) {
            return Ok(None);
        }
        Ok(string_prototype::fast_primitive_char_code_at(
            recv_value,
            args,
            &mut self.gc_heap,
        ))
    }

    fn try_fast_primitive_number_to_string(
        &mut self,
        recv_value: Value,
        args: &[Value],
    ) -> Result<Option<Value>, VmError> {
        let Some(proto) = self.constructor_prototype_value("Number")?.as_object() else {
            return Ok(None);
        };
        let value = match crate::object::lookup(proto, &self.gc_heap, "toString") {
            crate::object::PropertyLookup::Data { value, .. } => value,
            crate::object::PropertyLookup::Accessor { .. }
            | crate::object::PropertyLookup::Absent => {
                return Ok(None);
            }
        };
        if !number::prototype::is_to_string_builtin(value, &self.gc_heap) {
            return Ok(None);
        }
        Ok(number::prototype::fast_primitive_to_string(
            recv_value,
            args,
            &mut self.gc_heap,
        ))
    }

    /// Resolve a method by receiver shape through the call site's load IC.
    ///
    /// Returns the method value on an IC hit or a freshly installed
    /// monomorphic data-slot candidate (own or direct-prototype), exactly the
    /// values [`Self::drive_load_property`] caches. Returns `None` when the
    /// property is not an IC-cacheable data slot — an accessor, a deeper
    /// prototype hop, or absent — so the caller falls back to the full
    /// `[[Get]]` method-resolution path that handles those cases.
    /// Well-known prototype object whose own-data method slots back a primitive
    /// receiver's builtin methods: `%String.prototype%` for a string,
    /// `%Number.prototype%` for a number. `None` for any other receiver (no
    /// primitive-method IC applies).
    fn primitive_method_proto(&self, recv: Value) -> Option<crate::object::JsObject> {
        if recv.is_string() {
            self.realm_intrinsics.string_prototype
        } else if recv.is_number() {
            self.realm_intrinsics.number_prototype
        } else {
            None
        }
    }

    pub(crate) fn resolve_method_ic(
        &mut self,
        obj: crate::object::JsObject,
        key: AtomizedPropertyKey<'_>,
        site: usize,
    ) -> Option<Value> {
        if site >= self.load_property_ics.len() || self.load_property_ics[site].is_megamorphic() {
            return None;
        }
        let mut hit_value: Option<Value> = None;
        for ic in self.load_property_ics[site].entries() {
            if let Some(value) = ic.load(obj, &self.gc_heap, key) {
                hit_value = Some(value);
                break;
            }
        }
        if let Some(value) = hit_value {
            self.property_ic_stats.record_hit(PropertyIcKind::Load);
            return Some(value);
        }
        if self.load_property_ics[site].entry_count() > 0 {
            self.load_property_ics[site]
                .record_guard_miss_with_stats(&mut self.property_ic_stats, PropertyIcKind::Load);
        } else {
            self.load_property_ics[site]
                .record_uncached_miss_with_stats(&mut self.property_ic_stats, PropertyIcKind::Load);
        }
        if !self.load_property_ics[site].is_megamorphic()
            && let Some((ic, value)) = LoadPropertyIc::install_candidate(obj, &self.gc_heap, key)
        {
            self.load_property_ics[site].install_with_stats(
                &mut self.property_ic_stats,
                PropertyIcKind::Load,
                ic,
            );
            return Some(value);
        }
        None
    }

    fn callable_has_own_function_method_shadow(
        &mut self,
        context: &ExecutionContext,
        recv_value: Value,
        name: &str,
    ) -> Result<bool, VmError> {
        if let Some(function_id) = recv_value.as_function().or_else(|| {
            recv_value
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        }) {
            let owner = recv_value.as_closure(&self.gc_heap);
            return self.ordinary_function_has_own_string_property_for_extensibility(
                context,
                owner,
                function_id,
                name,
            );
        }
        if let Some(native) = recv_value.as_native_function() {
            return Ok(native
                .own_property_descriptor(&mut self.gc_heap, name)
                .ok()
                .flatten()
                .is_some());
        }
        if let Some(bound) = recv_value.as_bound_function() {
            return Ok(crate::function_metadata::bound_has_own_property(
                &bound,
                &self.gc_heap,
                name,
            ));
        }
        if let Some(obj) = recv_value.as_object() {
            return Ok(crate::object::get_own_descriptor(obj, &self.gc_heap, name).is_some());
        }
        Ok(false)
    }

    fn ordinary_method_value_for_call(
        &mut self,
        context: &ExecutionContext,
        recv_value: Value,
        name: &str,
    ) -> Result<Value, VmError> {
        let key = VmPropertyKey::String(name);
        match self.ordinary_get_value(context, recv_value, recv_value, &key, 0)? {
            VmGetOutcome::Value(value) => Ok(value),
            VmGetOutcome::InvokeGetter { getter } => {
                let args: SmallVec<[Value; 8]> = SmallVec::new();
                self.run_callable_sync(context, &getter, recv_value, args)
            }
        }
    }

    fn typed_array_own_method_value_for_call(
        &mut self,
        context: &ExecutionContext,
        recv_value: Value,
        name: &str,
    ) -> Result<Option<Value>, VmError> {
        let Some(t) = recv_value.as_typed_array(&self.gc_heap) else {
            return Ok(None);
        };
        let Some(bag) = t.expando(&self.gc_heap) else {
            return Ok(None);
        };
        match crate::object::lookup_own(bag, &self.gc_heap, name) {
            crate::object::PropertyLookup::Data { value, .. } => Ok(Some(value)),
            crate::object::PropertyLookup::Accessor { getter, .. } => match getter {
                Some(getter) if self.is_callable_runtime(&getter) => Ok(Some(
                    self.run_callable_sync(context, &getter, recv_value, SmallVec::new())?,
                )),
                _ => Ok(Some(Value::undefined())),
            },
            crate::object::PropertyLookup::Absent => Ok(None),
        }
    }

    /// Stage-4 `GetMethod` bridge for the slow `CallMethodValue`
    /// fallback. Builtin fast arms still live above this helper; this
    /// routine centralizes the ordinary property/getter path so the call
    /// opcode can be collapsed behind one `GetMethod + Call` boundary in
    /// smaller, reviewable steps.
    fn get_method_value_for_call(
        &mut self,
        context: &ExecutionContext,
        stack: &mut HoltStack,
        recv_value: Value,
        name: &str,
    ) -> Result<Option<Value>, VmError> {
        let is_property_bearing = recv_value.is_object()
            || recv_value.is_proxy()
            || recv_value.is_array()
            || recv_value.is_regexp()
            || recv_value.is_map()
            || recv_value.is_set()
            || recv_value.is_weak_map()
            || recv_value.is_weak_set()
            || recv_value.is_weak_ref()
            || recv_value.is_finalization_registry()
            || recv_value.is_promise()
            || recv_value.is_array_buffer()
            || recv_value.is_data_view()
            || recv_value.is_typed_array()
            || recv_value.is_intl()
            || recv_value.is_generator()
            || recv_value.is_iterator();
        if is_property_bearing {
            // Property-bearing exotic receivers route through
            // `ordinary_get_value` so user-installed own properties
            // shadow the builtin fallback path.
            let key = VmPropertyKey::String(name);
            return match self.ordinary_get_value(context, recv_value, recv_value, &key, 0)? {
                VmGetOutcome::Value(value) => Ok(Some(value)),
                VmGetOutcome::InvokeGetter { getter } => {
                    let args: SmallVec<[Value; 8]> = SmallVec::new();
                    Ok(Some(
                        self.run_callable_sync(context, &getter, recv_value, args)?,
                    ))
                }
            };
        }
        if let Some(c) = recv_value.as_class_constructor() {
            let value = if name == "prototype" {
                Value::object(c.prototype(&self.gc_heap))
            } else {
                // Go through the full `[[Get]]` ladder so accessor
                // descriptors on static members invoke their getter.
                let statics = Value::object(c.statics(&self.gc_heap));
                let key = VmPropertyKey::String(name);
                match self.ordinary_get_value(context, statics, statics, &key, 0)? {
                    VmGetOutcome::Value(v) => v,
                    VmGetOutcome::InvokeGetter { getter } => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.run_callable_sync(context, &getter, statics, args)?
                    }
                }
            };
            return Ok(Some(value));
        }
        if let Some(fid) = recv_value.as_function().or_else(|| {
            recv_value
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        }) {
            // §10.1.8 OrdinaryGet on a callable receiver — user
            // properties resolve via the function-properties side table.
            let owner = recv_value.as_closure(&self.gc_heap);
            return Ok(Some(
                self.function_property_get_stack_rooted_with_receiver(
                    context,
                    stack,
                    owner,
                    fid,
                    Some(recv_value),
                    name,
                )?,
            ));
        }
        if let Some(native) = recv_value.as_native_function() {
            // Native callable receiver — own properties first, then the
            // §10.1.8 OrdinaryGet walk up `%Function.prototype%` /
            // `%Object.prototype%` so a user-installed method (e.g.
            // `Function.prototype.slice = String.prototype.slice`)
            // resolves for calls exactly as it does for property reads.
            let value = match native
                .own_property_descriptor(&mut self.gc_heap, name)?
                .map(|desc| descriptor_value(&desc))
            {
                Some(value) => value,
                // §10.1.8 — explicit [[Prototype]] chain (per-kind
                // TypedArray ctor → %TypedArray%) resolves inherited
                // statics before the %Function.prototype% fallback.
                None => match native.prototype_override(&self.gc_heap) {
                    Some(parent) => {
                        let key = VmPropertyKey::String(name);
                        match self.ordinary_get_value(context, parent, recv_value, &key, 0)? {
                            VmGetOutcome::Value(value) => value,
                            VmGetOutcome::InvokeGetter { getter } => self.run_callable_sync(
                                context,
                                &getter,
                                recv_value,
                                SmallVec::new(),
                            )?,
                        }
                    }
                    None => self
                        .load_function_prototype_method(name)
                        .or_else(|| self.load_object_prototype_method(name))
                        .unwrap_or_else(Value::undefined),
                },
            };
            return Ok(Some(value));
        }
        if recv_value.is_boolean()
            || recv_value.is_number()
            || recv_value.is_string()
            || recv_value.is_symbol()
            || recv_value.is_big_int()
            || recv_value.is_temporal()
        {
            // §7.1.18 ToObject — primitive receivers walk the
            // constructor's prototype to surface inherited
            // `Object.prototype.*` methods.
            let key = VmPropertyKey::String(name);
            return match self.ordinary_get_value(context, recv_value, recv_value, &key, 0)? {
                VmGetOutcome::Value(value) => Ok(Some(value)),
                VmGetOutcome::InvokeGetter { getter } => {
                    let args: SmallVec<[Value; 8]> = SmallVec::new();
                    Ok(Some(
                        self.run_callable_sync(context, &getter, recv_value, args)?,
                    ))
                }
            };
        }
        Ok(None)
    }

    /// §22.1.3.18 / §22.1.3.19 callable replaceValue path. Walks
    /// the receiver string's UTF-16 units, locates each
    /// non-overlapping match of the (String-coerced) needle, and
    /// invokes the callback with `(matched, position, fullString)`
    /// per spec step 6.h. Returns the spliced result string.
    pub(crate) fn dispatch_string_callable_replace(
        &mut self,
        context: &ExecutionContext,
        receiver: &Value,
        args: &SmallVec<[Value; 8]>,
        replace_all: bool,
    ) -> Result<Value, VmError> {
        use crate::string::JsString;
        let recv = receiver
            .as_string(&self.gc_heap)
            .ok_or(VmError::TypeMismatch)?;
        let needle = args
            .first()
            .and_then(|v| v.as_string(&self.gc_heap))
            .ok_or(VmError::TypeMismatch)?;
        let callback = args.get(1).cloned().unwrap_or(Value::undefined());
        let recv_units = recv.to_utf16_vec(&self.gc_heap);
        let needle_units = needle.to_utf16_vec(&self.gc_heap);
        let needle_len = needle_units.len();
        let recv_value = Value::string(recv);
        let mut out: Vec<u16> = Vec::with_capacity(recv_units.len());
        if needle_len == 0 {
            let positions: Vec<usize> = if replace_all {
                (0..=recv_units.len()).collect()
            } else {
                vec![0]
            };
            for pos in positions {
                let cb_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                    Value::string(needle),
                    Value::number_f64(pos as f64),
                    recv_value,
                ];
                let raw =
                    self.run_callable_sync(context, &callback, Value::undefined(), cb_args)?;
                let raw_string = if let Some(s) = raw.as_string(&self.gc_heap) {
                    s
                } else {
                    JsString::from_str(&raw.display_string(&self.gc_heap), &mut self.gc_heap)
                        .map_err(|_| VmError::TypeMismatch)?
                };
                out.extend_from_slice(&raw_string.to_utf16_vec(&self.gc_heap));
                if pos < recv_units.len() {
                    out.push(recv_units[pos]);
                }
            }
            return Ok(Value::string(
                JsString::from_utf16_units(&out, &mut self.gc_heap)
                    .map_err(|_| VmError::TypeMismatch)?,
            ));
        }
        if recv_units.len() < needle_len {
            // Needle longer than receiver — no match possible.
            return Ok(Value::string(recv));
        }
        let last_start = recv_units.len() - needle_len;
        let mut cursor: usize = 0;
        while cursor <= last_start {
            if recv_units[cursor..cursor + needle_len] == needle_units[..] {
                let cb_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                    Value::string(needle),
                    Value::number_f64(cursor as f64),
                    recv_value,
                ];
                let raw =
                    self.run_callable_sync(context, &callback, Value::undefined(), cb_args)?;
                let raw_string = if let Some(s) = raw.as_string(&self.gc_heap) {
                    s
                } else {
                    JsString::from_str(&raw.display_string(&self.gc_heap), &mut self.gc_heap)
                        .map_err(|_| VmError::TypeMismatch)?
                };
                out.extend_from_slice(&raw_string.to_utf16_vec(&self.gc_heap));
                cursor += needle_len;
                if !replace_all {
                    break;
                }
            } else {
                out.push(recv_units[cursor]);
                cursor += 1;
            }
        }
        out.extend_from_slice(&recv_units[cursor..]);
        Ok(Value::string(
            JsString::from_utf16_units(&out, &mut self.gc_heap)
                .map_err(|_| VmError::TypeMismatch)?,
        ))
    }

    /// §23.2.3 TypedArray prototype callback methods —
    /// `forEach` / `map` / `filter` / `find` / `findIndex` /
    /// `findLast` / `findLastIndex` / `every` / `some` / `reduce` /
    /// `reduceRight`. Same shape as the Array prototype family but
    /// element snapshots come from the TypedArray's backing buffer
    /// and `map` / `filter` allocate a fresh TypedArray of the
    /// receiver's kind.
    ///
    /// <https://tc39.es/ecma262/#sec-typedarray.prototype-objects>
    #[allow(clippy::too_many_arguments)]
    /// §23.2.3 TypedArray prototype callback methods, value-returning
    /// form for the real-native dispatch path (`bootstrap_typed_array`'s
    /// `ta_*` callback wrappers call this through `NativeCtx`). Mirrors
    /// the Array callback driver: elements are snapshot once, then each
    /// callback re-enters through `run_callable_sync`. For `map` /
    /// `filter` the species result is allocated per §23.2.3.20 / .10 and
    /// pinned on the iteration-anchor stack so a GC triggered inside a
    /// callback cannot reclaim it.
    /// Live per-iteration element read for callback-driven
    /// TypedArray methods (§23.2.3 — `Get(O, ToString(k))` each
    /// step): a buffer detached or shrunk mid-iteration yields
    /// `undefined`, and writes from earlier callbacks are observed.
    pub(crate) fn ta_live_element(
        &mut self,
        t: &crate::binary::typed_array::JsTypedArray,
        i: usize,
    ) -> Result<Value, VmError> {
        if t.buffer(&self.gc_heap).is_detached(&self.gc_heap) || i >= t.length(&self.gc_heap) {
            return Ok(Value::undefined());
        }
        t.get(&mut self.gc_heap, i).map_err(crate::oom_to_vm)
    }

    fn run_typed_array_callback(
        &mut self,
        lean: &mut Option<LeanCallbackState>,
        context: &ExecutionContext,
        callee: Value,
        this_arg: Value,
        args: &[Value],
    ) -> Result<Value, VmError> {
        match lean {
            Some(inner) => {
                self.run_bytecode_callable_committed_lean_args(inner, context, this_arg, args)
            }
            None => {
                let mut owned: SmallVec<[Value; 8]> = SmallVec::with_capacity(args.len());
                owned.extend(args.iter().copied());
                self.run_callable_sync(context, &callee, this_arg, owned)
            }
        }
    }

    pub(crate) fn typed_array_callback_value_dispatch(
        &mut self,
        context: &ExecutionContext,
        t: &crate::binary::typed_array::JsTypedArray,
        name: &str,
        args: &[Value],
    ) -> Result<Value, VmError> {
        let ta_value = Value::typed_array(*t);
        // §23.2.4.4 ValidateTypedArray — a detached or out-of-bounds
        // backing buffer (a fixed-length view whose resizable buffer
        // shrank past its end) throws before the length read or any
        // callback runs.
        if t.is_out_of_bounds(&self.gc_heap) {
            return Err(self.err_type(
                (format!(
                    "TypedArray.prototype.{name} called on a detached or out-of-bounds ArrayBuffer"
                ))
                .into(),
            ));
        }
        let len = t.length(&self.gc_heap);
        let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
        let callee = require_callable(args.first())?;
        let mut lean = self.acquire_lean_callback_stack(context, callee);

        let result =
            (|interp: &mut Self, lean: &mut Option<LeanCallbackState>| -> Result<Value, VmError> {
                match name {
                    "forEach" => {
                        for i in 0..len {
                            let value = interp.ta_live_element(t, i)?;
                            interp.run_typed_array_callback(
                                lean,
                                context,
                                callee,
                                this_arg,
                                &[
                                    value,
                                    Value::number(NumberValue::from_i32(i as i32)),
                                    ta_value,
                                ],
                            )?;
                        }
                        Ok(Value::undefined())
                    }
                    "map" => {
                        // §23.2.3.20 — `A = ? TypedArraySpeciesCreate(O, « len »)`
                        // (step 5) runs before any callback. `A` is pinned on the
                        // iteration-anchor stack so it stays GC-rooted across each
                        // callback re-entry.
                        let a = interp.typed_array_species_create(context, t, len)?;
                        let a_value = Value::typed_array(a);
                        let target_kind = a.kind();
                        let anchor = interp.push_iteration_anchor(a_value);
                        let result = (|interp: &mut Self| -> Result<(), VmError> {
                            for i in 0..len {
                                let value = interp.ta_live_element(t, i)?;
                                let mapped = interp.run_typed_array_callback(
                                    lean,
                                    context,
                                    callee,
                                    this_arg,
                                    &[
                                        value,
                                        Value::number(NumberValue::from_i32(i as i32)),
                                        ta_value,
                                    ],
                                )?;
                                let coerced = crate::binary::dispatch::coerce_element_for_store(
                                    &mut interp.gc_heap,
                                    target_kind,
                                    &mapped,
                                )?;
                                a.set(&mut interp.gc_heap, i, &coerced);
                            }
                            Ok(())
                        })(interp);
                        interp.pop_iteration_anchors_to(anchor - 1);
                        result?;
                        Ok(a_value)
                    }
                    "filter" => {
                        // §23.2.3.10 — run the predicate over every element,
                        // collecting kept values, then call
                        // `TypedArraySpeciesCreate(O, « captured »)` (step 9) with
                        // the kept count and copy the survivors in.
                        let mut kept: Vec<Value> = Vec::new();
                        for i in 0..len {
                            let value = interp.ta_live_element(t, i)?;
                            let selected = interp.run_typed_array_callback(
                                lean,
                                context,
                                callee,
                                this_arg,
                                &[
                                    value,
                                    Value::number(NumberValue::from_i32(i as i32)),
                                    ta_value,
                                ],
                            )?;
                            if selected.to_boolean(&interp.gc_heap) {
                                kept.push(value);
                            }
                        }
                        let a = interp.typed_array_species_create(context, t, kept.len())?;
                        let target_kind = a.kind();
                        for (i, value) in kept.iter().enumerate() {
                            let coerced = crate::binary::dispatch::coerce_element_for_store(
                                &mut interp.gc_heap,
                                target_kind,
                                value,
                            )?;
                            a.set(&mut interp.gc_heap, i, &coerced);
                        }
                        Ok(Value::typed_array(a))
                    }
                    "find" => {
                        let mut found = Value::undefined();
                        for i in 0..len {
                            let value = interp.ta_live_element(t, i)?;
                            let hit = interp.run_typed_array_callback(
                                lean,
                                context,
                                callee,
                                this_arg,
                                &[
                                    value,
                                    Value::number(NumberValue::from_i32(i as i32)),
                                    ta_value,
                                ],
                            )?;
                            if hit.to_boolean(&interp.gc_heap) {
                                found = value;
                                break;
                            }
                        }
                        Ok(found)
                    }
                    "findIndex" => {
                        let mut idx: i32 = -1;
                        for i in 0..len {
                            let value = interp.ta_live_element(t, i)?;
                            let hit = interp.run_typed_array_callback(
                                lean,
                                context,
                                callee,
                                this_arg,
                                &[
                                    value,
                                    Value::number(NumberValue::from_i32(i as i32)),
                                    ta_value,
                                ],
                            )?;
                            if hit.to_boolean(&interp.gc_heap) {
                                idx = i as i32;
                                break;
                            }
                        }
                        Ok(Value::number_i32(idx))
                    }
                    "findLast" => {
                        let mut found = Value::undefined();
                        for i in (0..len).rev() {
                            let value = interp.ta_live_element(t, i)?;
                            let hit = interp.run_typed_array_callback(
                                lean,
                                context,
                                callee,
                                this_arg,
                                &[
                                    value,
                                    Value::number(NumberValue::from_i32(i as i32)),
                                    ta_value,
                                ],
                            )?;
                            if hit.to_boolean(&interp.gc_heap) {
                                found = value;
                                break;
                            }
                        }
                        Ok(found)
                    }
                    "findLastIndex" => {
                        let mut idx: i32 = -1;
                        for i in (0..len).rev() {
                            let value = interp.ta_live_element(t, i)?;
                            let hit = interp.run_typed_array_callback(
                                lean,
                                context,
                                callee,
                                this_arg,
                                &[
                                    value,
                                    Value::number(NumberValue::from_i32(i as i32)),
                                    ta_value,
                                ],
                            )?;
                            if hit.to_boolean(&interp.gc_heap) {
                                idx = i as i32;
                                break;
                            }
                        }
                        Ok(Value::number_i32(idx))
                    }
                    "every" => {
                        let mut all = true;
                        for i in 0..len {
                            let value = interp.ta_live_element(t, i)?;
                            let hit = interp.run_typed_array_callback(
                                lean,
                                context,
                                callee,
                                this_arg,
                                &[
                                    value,
                                    Value::number(NumberValue::from_i32(i as i32)),
                                    ta_value,
                                ],
                            )?;
                            if !hit.to_boolean(&interp.gc_heap) {
                                all = false;
                                break;
                            }
                        }
                        Ok(Value::boolean(all))
                    }
                    "some" => {
                        let mut any = false;
                        for i in 0..len {
                            let value = interp.ta_live_element(t, i)?;
                            let hit = interp.run_typed_array_callback(
                                lean,
                                context,
                                callee,
                                this_arg,
                                &[
                                    value,
                                    Value::number(NumberValue::from_i32(i as i32)),
                                    ta_value,
                                ],
                            )?;
                            if hit.to_boolean(&interp.gc_heap) {
                                any = true;
                                break;
                            }
                        }
                        Ok(Value::boolean(any))
                    }
                    "reduce" | "reduceRight" => {
                        let has_init = args.len() >= 2;
                        let reverse = name == "reduceRight";
                        if len == 0 && !has_init {
                            return Err(VmError::TypeMismatch);
                        }
                        let step: i64 = if reverse { -1 } else { 1 };
                        let (mut acc, start_idx) = if has_init {
                            (args[1], if reverse { len as i64 - 1 } else { 0 })
                        } else {
                            let seed = if reverse { len - 1 } else { 0 };
                            (interp.ta_live_element(t, seed)?, seed as i64 + step)
                        };
                        let mut i = start_idx;
                        while i >= 0 && (i as usize) < len {
                            let value = interp.ta_live_element(t, i as usize)?;
                            acc = interp.run_typed_array_callback(
                                lean,
                                context,
                                callee,
                                Value::undefined(),
                                &[
                                    acc,
                                    value,
                                    Value::number(NumberValue::from_i32(i as i32)),
                                    ta_value,
                                ],
                            )?;
                            i += step;
                        }
                        Ok(acc)
                    }
                    _ => Err(VmError::TypeMismatch),
                }
            })(self, &mut lean);
        self.release_lean_callback_stack(lean);
        result
    }

    /// §23.2.4.1 `TypedArraySpeciesCreate(exemplar, « length »)`.
    /// Resolves `SpeciesConstructor(exemplar, %DefaultConstructor%)`
    /// (§7.3.22) — observing a user `constructor` / `@@species`
    /// override — then performs `TypedArrayCreate(constructor,
    /// « length »)` (§23.2.4.2) and validates the result is a
    /// non-detached TypedArray of at least `length` elements.
    fn typed_array_species_create(
        &mut self,
        context: &ExecutionContext,
        exemplar: &crate::binary::typed_array::JsTypedArray,
        length: usize,
    ) -> Result<crate::binary::typed_array::JsTypedArray, VmError> {
        let mut argv: SmallVec<[Value; 8]> = SmallVec::new();
        argv.push(Value::number(NumberValue::from_f64(length as f64)));
        self.typed_array_create_via_species(context, exemplar, argv, Some(length))
    }

    /// §23.2.4.2 `TypedArrayCreate(SpeciesConstructor(exemplar), argv)`.
    /// Shared core for the length form (`map` / `filter` / `slice`,
    /// `min_length = Some`) and the `« buffer, byteOffset, length »`
    /// form (`subarray`, `min_length = None`): resolves the species
    /// constructor, constructs the result, and validates it is a
    /// non-detached TypedArray (plus the `[[ArrayLength]] >= length`
    /// check that only applies when the argument list is a single
    /// Number).
    fn typed_array_create_via_species(
        &mut self,
        context: &ExecutionContext,
        exemplar: &crate::binary::typed_array::JsTypedArray,
        argv: SmallVec<[Value; 8]>,
        min_length: Option<usize>,
    ) -> Result<crate::binary::typed_array::JsTypedArray, VmError> {
        let exemplar_value = Value::typed_array(*exemplar);
        let default_name = exemplar.kind().name();
        let default_ctor = crate::object::get(self.global_this, &self.gc_heap, default_name)
            .ok_or_else(|| {
                self.err_type((format!("%{default_name}% intrinsic is missing")).into())
            })?;
        let constructor =
            self.species_constructor_value(context, &exemplar_value, &default_ctor)?;
        let result = self.run_construct_sync(context, &constructor, constructor, argv)?;
        let Some(new_ta) = result.as_typed_array(&self.gc_heap) else {
            return Err(self
                .err_type(("Species constructor did not return a TypedArray".to_string()).into()));
        };
        if new_ta.buffer(&self.gc_heap).is_detached(&self.gc_heap) {
            return Err(self.err_type(
                ("Species constructor returned a TypedArray with a detached buffer".to_string())
                    .into(),
            ));
        }
        if let Some(min) = min_length
            && new_ta.length(&self.gc_heap) < min
        {
            return Err(self.err_type(
                ("Species constructor returned a TypedArray smaller than required".to_string())
                    .into(),
            ));
        }
        Ok(new_ta)
    }

    /// §23.2.3.27 `%TypedArray%.prototype.subarray(begin, end)`. Builds
    /// a new view over the *same* buffer: `begin` / `end` coerce
    /// through `ToIntegerOrInfinity`, then
    /// `TypedArraySpeciesCreate(O, « buffer, beginByteOffset, length »)`
    /// (the buffer form, so no result-length check) allocates the view.
    /// §23.2.3.27 `%TypedArray%.prototype.subarray(begin, end)`,
    /// value-returning form for the real-native dispatch path. Builds a
    /// new view over the *same* buffer through `TypedArraySpeciesCreate`
    /// (the buffer form); `begin` / `end` coerce through
    /// `ToIntegerOrInfinity`, observing user `@@toPrimitive` / `valueOf`.
    pub(crate) fn typed_array_subarray_value_dispatch(
        &mut self,
        context: &ExecutionContext,
        t: &crate::binary::typed_array::JsTypedArray,
        args: &[Value],
    ) -> Result<Value, VmError> {
        let buffer = t.buffer(&self.gc_heap);
        // §23.2.3.27 step 4 — `[[ArrayLength]]` is `0` for a detached
        // buffer; `subarray` does not itself throw on detachment.
        let src_len = t.length(&self.gc_heap) as i64;
        let begin = self.integer_or_infinity_for_arg(context, args.first())?;
        let begin_index = relative_index_clamp(begin, src_len);
        let relative_end = match args.get(1) {
            None => src_len as f64,
            Some(v) if v.is_undefined() => src_len as f64,
            Some(_) => self.integer_or_infinity_for_arg(context, args.get(1))?,
        };
        let end_index = relative_index_clamp(relative_end, src_len);
        let new_length = (end_index - begin_index).max(0) as usize;
        let bpe = t.kind().bytes_per_element();
        // §23.2.3.30 step 13 — [[ByteOffset]] is the construction-time
        // slot; the `end` coercion may have detached the buffer, which
        // zeroes the public accessor but not the slot.
        let begin_byte_offset = t.raw_byte_offset(&self.gc_heap) + begin_index as usize * bpe;

        let mut argv: SmallVec<[Value; 8]> = SmallVec::new();
        argv.push(Value::array_buffer(buffer));
        argv.push(Value::number(NumberValue::from_f64(
            begin_byte_offset as f64,
        )));
        // §23.2.3.30 — a length-tracking source with no end argument
        // produces a length-tracking view: the species constructor is
        // called WITHOUT the length argument.
        let end_absent = args.get(1).is_none() || args.get(1).is_some_and(|v| v.is_undefined());
        if !(end_absent && t.is_length_tracking(&self.gc_heap)) {
            argv.push(Value::number(NumberValue::from_f64(new_length as f64)));
        }
        let a = self.typed_array_create_via_species(context, t, argv, None)?;
        Ok(Value::typed_array(a))
    }

    /// §23.2.3.26 `%TypedArray%.prototype.slice(start, end)`,
    /// value-returning form for the real-native dispatch path. Coerces
    /// both operands through `ToIntegerOrInfinity` (observing user
    /// `@@toPrimitive` / `valueOf`), allocates the result via
    /// `TypedArraySpeciesCreate(O, « count »)`, then copies the in-range
    /// elements. The source buffer is re-checked for detachment after the
    /// (potentially re-entrant) species constructor runs; the element
    /// copy itself does not re-enter, so the result stays live as a local.
    pub(crate) fn typed_array_slice_value_dispatch(
        &mut self,
        context: &ExecutionContext,
        t: &crate::binary::typed_array::JsTypedArray,
        args: &[Value],
    ) -> Result<Value, VmError> {
        if t.is_out_of_bounds(&self.gc_heap) {
            return Err(self.err_type(
                ("Cannot slice a detached or out-of-bounds TypedArray".to_string()).into(),
            ));
        }
        let len = t.length(&self.gc_heap) as i64;
        let start = self.integer_or_infinity_for_arg(context, args.first())?;
        let k = relative_index_clamp(start, len);
        let relative_end = match args.get(1) {
            None => len as f64,
            Some(v) if v.is_undefined() => len as f64,
            Some(_) => self.integer_or_infinity_for_arg(context, args.get(1))?,
        };
        let final_index = relative_index_clamp(relative_end, len);
        let count = (final_index - k).max(0) as usize;

        let a = self.typed_array_species_create(context, t, count)?;
        if count > 0 {
            // §23.2.3.27 step 11 — the argument coercion and the species
            // constructor can both resize the source. Re-validate: an
            // out-of-bounds (or detached) source throws; otherwise clamp
            // the copy to the source's current length, leaving the tail
            // of the freshly-created (zeroed) result untouched.
            if t.is_out_of_bounds(&self.gc_heap) {
                return Err(self.err_type(
                    ("TypedArray buffer was detached or resized out of bounds during slice"
                        .to_string())
                    .into(),
                ));
            }
            let base = k as usize;
            let cur_len = t.length(&self.gc_heap);
            let copy_count = count.min(cur_len.saturating_sub(base));
            let target_kind = a.kind();
            for n in 0..copy_count {
                let value = t
                    .get(&mut self.gc_heap, base + n)
                    .map_err(crate::oom_to_vm)?;
                let coerced = crate::binary::dispatch::coerce_element_for_store(
                    &mut self.gc_heap,
                    target_kind,
                    &value,
                )?;
                a.set(&mut self.gc_heap, n, &coerced);
            }
        }
        Ok(Value::typed_array(a))
    }

    /// §7.1.5 `ToIntegerOrInfinity` applied to an optional argument
    /// (missing / `undefined` → `0`). Re-enters user `@@toPrimitive`
    /// / `valueOf` via `coerce_to_number` and raises TypeError for
    /// Symbol / BigInt operands.
    pub(crate) fn integer_or_infinity_for_arg(
        &mut self,
        context: &ExecutionContext,
        arg: Option<&Value>,
    ) -> Result<f64, VmError> {
        let n = match arg {
            None => return Ok(0.0),
            Some(v) if v.is_undefined() => return Ok(0.0),
            Some(v) => self.coerce_to_number(context, v)?.as_f64(),
        };
        if n.is_nan() {
            Ok(0.0)
        } else if n.is_infinite() {
            Ok(n)
        } else {
            Ok(n.trunc())
        }
    }

    fn dispatch_function_method(
        &mut self,
        stack: &mut HoltStack,
        context: &ExecutionContext,
        callee: &Value,
        name: &str,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        match name {
            "call" => {
                let mut iter = args.into_iter();
                let this_value = iter.next().unwrap_or(Value::undefined());
                let forwarded: SmallVec<[Value; 8]> = iter.collect();
                stack[top_idx].advance_pc(self.current_byte_len)?;
                self.invoke(stack, context, callee, this_value, forwarded, dst)
            }
            "apply" => {
                let mut iter = args.into_iter();
                let this_value = iter.next().unwrap_or(Value::undefined());
                let forwarded: SmallVec<[Value; 8]> = match iter.next() {
                    None => SmallVec::new(),
                    Some(v) if v.is_nullish() => SmallVec::new(),
                    Some(arg_array) => self.create_list_from_array_like(context, arg_array)?,
                };
                stack[top_idx].advance_pc(self.current_byte_len)?;
                self.invoke(stack, context, callee, this_value, forwarded, dst)
            }
            "bind" => {
                let mut iter = args.into_iter();
                let this_value = iter.next().unwrap_or(Value::undefined());
                let bound_args: SmallVec<[Value; 4]> = iter.collect();
                let target = *callee;
                let pc = stack[top_idx].pc;
                match self.callable_bind_metadata_get(context, &target, "name")? {
                    BindMetadataGet::Value(target_name) => self.continue_bind_function_after_name(
                        stack,
                        context,
                        dst,
                        target,
                        this_value,
                        bound_args,
                        target_name,
                    ),
                    BindMetadataGet::Getter(getter) => {
                        self.frame_ensure_cold(&mut stack[top_idx])
                            .pending_bind_function = Some(PendingBindFunction {
                            pc,
                            dst,
                            target,
                            bound_this: this_value,
                            bound_args,
                            stage: PendingBindStage::Name,
                            target_name: None,
                        });
                        self.invoke(stack, context, &getter, target, SmallVec::new(), dst)
                    }
                }
            }
            // §20.2.3.5 Function.prototype.toString — foundation
            // returns the canonical `function <name>() { [native
            // code] }` placeholder. Spec mandates a source-faithful
            // representation when source is available; the
            // foundation defers source preservation to a follow-up.
            // <https://tc39.es/ecma262/#sec-function.prototype.tostring>
            "toString" => {
                // §20.2.3.5 step 1 — throw a TypeError when `this` is not
                // callable (e.g. a Proxy wrapping a non-callable target).
                if !self.is_callable_runtime(callee) {
                    return Err(VmError::NotCallable);
                }
                let display = {
                    let owner_bag = self.callable_bag_for_value(callee);
                    let mut ctx = function_metadata::FunctionMetadataContext::new(
                        context,
                        &mut self.gc_heap,
                        owner_bag,
                        &self.function_deleted_metadata,
                    );
                    function_metadata::callable_to_string(&mut ctx, callee)
                };
                let s = JsString::from_str(&display, &mut self.gc_heap)
                    .map_err(|_| VmError::TypeMismatch)?;
                let frame = &mut stack[top_idx];
                write_register(frame, dst, Value::string(s))?;
                frame.advance_pc(self.current_byte_len)?;
                Ok(())
            }
            _ => Err(self.err_unknown_intrinsic(name.to_string().into())),
        }
    }
}
