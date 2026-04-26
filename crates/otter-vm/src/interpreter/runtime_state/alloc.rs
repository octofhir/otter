//! Heap allocation (objects, arrays, strings, BigInts, RegExps, symbols,
//! host functions), `gc_safepoint`, global / burrow installation, closure
//! allocation, and the `is_ecma_object` / `is_constructible` predicates
//! used for IsCallable / IsConstructor.

use core::any::Any;

use crate::builders::{BurrowBuilder, ObjectMemberPlan};
use crate::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
use crate::host::HostFunctionId;
use crate::module::FunctionIndex;
use crate::object::{
    ClosureFlags as ObjectClosureFlags, HeapValueKind, ObjectError, ObjectHandle,
    PropertyAttributes, PropertyValue,
};
use crate::payload::VmTrace;
use crate::value::RegisterValue;

use super::{InterpreterError, RuntimeState};

fn vm_native_call_error_from_object(error: ObjectError) -> VmNativeCallError {
    match InterpreterError::from(error) {
        InterpreterError::OutOfMemory => {
            VmNativeCallError::Internal("out of memory: heap limit exceeded".into())
        }
        error => VmNativeCallError::Internal(format!("{error}").into()),
    }
}

fn vm_native_call_error_from_interpreter(error: InterpreterError) -> VmNativeCallError {
    match error {
        InterpreterError::OutOfMemory => {
            VmNativeCallError::Internal("out of memory: heap limit exceeded".into())
        }
        error => VmNativeCallError::Internal(format!("{error}").into()),
    }
}

impl RuntimeState {
    /// C6: acquires a `(registers, upvalues)` buffer pair sized for the
    /// next call. Both vectors are zero-filled (`RegisterValue::default()`
    /// / `None`) up to `register_count`. If the pool has a recycled buffer
    /// we reuse its capacity; otherwise we allocate fresh.
    ///
    /// Pair the call with [`Self::release_call_buffers`] when the frame
    /// pops, so the next call can reuse the allocation. Forgetting to
    /// release is a perf regression but never a correctness issue —
    /// the buffer just gets dropped with the activation.
    pub fn acquire_call_buffers(
        &mut self,
        register_count: usize,
    ) -> (Vec<RegisterValue>, Vec<Option<ObjectHandle>>) {
        let mut registers = self.register_buffer_pool.pop().unwrap_or_default();
        registers.clear();
        registers.resize(register_count, RegisterValue::default());

        let mut upvalues = self.upvalue_buffer_pool.pop().unwrap_or_default();
        upvalues.clear();
        upvalues.resize(register_count, None);

        (registers, upvalues)
    }

    /// C6: returns a pair of activation buffers to the per-runtime pool.
    /// Drops the buffers if the pool is at capacity, bounding the
    /// memory cost. Both vectors are cleared (not deallocated) on entry
    /// so reads from the pool always see a zero-filled buffer with the
    /// previous capacity preserved.
    pub fn release_call_buffers(
        &mut self,
        mut registers: Vec<RegisterValue>,
        mut upvalues: Vec<Option<ObjectHandle>>,
    ) {
        registers.clear();
        upvalues.clear();
        if self.register_buffer_pool.len() < crate::interpreter::CALL_BUFFER_POOL_CAPACITY {
            self.register_buffer_pool.push(registers);
        }
        if self.upvalue_buffer_pool.len() < crate::interpreter::CALL_BUFFER_POOL_CAPACITY {
            self.upvalue_buffer_pool.push(upvalues);
        }
    }

    /// GC safepoint — called at loop back-edges and function call boundaries.
    /// Collects roots from intrinsics and the provided register window,
    /// then triggers collection if memory pressure warrants it.
    pub fn gc_safepoint(&mut self, registers: &[RegisterValue]) {
        let mut roots = self.intrinsics().gc_root_handles();
        // Extract ObjectHandle roots from the current register window.
        for reg in registers {
            if let Some(handle) = reg.as_object_handle() {
                roots.push(ObjectHandle(handle));
            }
        }
        self.objects.maybe_collect_garbage(&roots);
    }

    /// Allocates one ordinary object with the runtime default prototype.
    pub fn alloc_object(&mut self) -> Result<ObjectHandle, InterpreterError> {
        let prototype = self.intrinsics().object_prototype();
        let handle = self.objects.alloc_object()?;
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("ordinary object prototype should exist");
        Ok(handle)
    }

    /// Allocates one ordinary object with an explicit prototype.
    pub fn alloc_object_with_prototype(
        &mut self,
        prototype: Option<ObjectHandle>,
    ) -> Result<ObjectHandle, InterpreterError> {
        let handle = self.objects.alloc_object()?;
        self.objects
            .set_prototype(handle, prototype)
            .expect("explicit object prototype should be valid");
        Ok(handle)
    }

    /// Allocates one ordinary object that carries a Rust-owned native payload.
    pub fn alloc_native_object<T>(&mut self, payload: T) -> Result<ObjectHandle, InterpreterError>
    where
        T: VmTrace + Any,
    {
        let prototype = self.intrinsics().object_prototype();
        self.alloc_native_object_with_prototype(Some(prototype), payload)
    }

    /// Allocates one payload-bearing object with an explicit prototype.
    pub fn alloc_native_object_with_prototype<T>(
        &mut self,
        prototype: Option<ObjectHandle>,
        payload: T,
    ) -> Result<ObjectHandle, InterpreterError>
    where
        T: VmTrace + Any,
    {
        let payload = self.native_payloads.insert(payload);
        let handle = self.objects.alloc_native_object(payload)?;
        self.objects
            .set_prototype(handle, prototype)
            .expect("explicit native object prototype should be valid");
        Ok(handle)
    }

    /// Allocates one dense array with the runtime default prototype.
    pub fn alloc_array(&mut self) -> Result<ObjectHandle, InterpreterError> {
        let prototype = self.intrinsics().array_prototype();
        let handle = self.objects.alloc_array()?;
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("array prototype should exist");
        Ok(handle)
    }

    /// Allocates an array and populates it with initial elements.
    pub fn alloc_array_with_elements(
        &mut self,
        elements: &[RegisterValue],
    ) -> Result<ObjectHandle, InterpreterError> {
        let handle = self.alloc_array()?;
        for &elem in elements {
            self.objects.push_element(handle, elem)?;
        }
        Ok(handle)
    }

    /// Extracts elements from an array handle into a Vec of RegisterValues.
    pub fn array_to_args(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<Vec<RegisterValue>, VmNativeCallError> {
        self.objects
            .array_elements(handle)
            .map_err(|e| VmNativeCallError::Internal(format!("array_to_args failed: {e:?}").into()))
    }

    pub fn list_from_array_like(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<Vec<RegisterValue>, VmNativeCallError> {
        let length_key = self.intern_property_name("length");
        let receiver = RegisterValue::from_object_handle(handle.0);
        let length_value = self.ordinary_get(handle, length_key, receiver)?;
        let length = usize::try_from(self.js_to_uint32(length_value).map_err(
            |error| match error {
                InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                InterpreterError::NativeCall(message) | InterpreterError::TypeError(message) => {
                    VmNativeCallError::Internal(message)
                }
                other => VmNativeCallError::Internal(format!("{other}").into()),
            },
        )?)
        .unwrap_or(usize::MAX);

        let mut values = Vec::with_capacity(length);
        for index in 0..length {
            let property = self.intern_property_name(&index.to_string());
            let value = self.ordinary_get(handle, property, receiver)?;
            values.push(value);
        }
        Ok(values)
    }

    /// Allocates one string object with the runtime default prototype.
    pub fn alloc_string(
        &mut self,
        value: impl Into<Box<str>>,
    ) -> Result<ObjectHandle, InterpreterError> {
        let prototype = self.intrinsics().string_prototype();
        let handle = self.objects.alloc_string(value)?;
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("string prototype should exist");
        Ok(handle)
    }

    /// Allocates a string from a WTF-16 `JsString` with the runtime default prototype.
    ///
    /// Preserves lone surrogates as-is.
    pub fn alloc_js_string(
        &mut self,
        value: crate::js_string::JsString,
    ) -> Result<ObjectHandle, InterpreterError> {
        let prototype = self.intrinsics().string_prototype();
        let handle = self.objects.alloc_js_string(value)?;
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("string prototype should exist");
        Ok(handle)
    }

    /// Allocates a string primitive and returns a [`RegisterValue`]
    /// tagged with [`crate::value::TAG_PTR_STRING`].
    ///
    /// The allocation goes through the page-based
    /// [`otter_gc::heap::GcHeap`] and the resulting
    /// `GcRef<JsStringGc>` is packed into the `RegisterValue`'s
    /// NaN-box payload — the production-grade default path
    /// (Strategy B).
    ///
    /// # Rooting contract
    ///
    /// While Phase 4–5 GC features (incremental marking, scavenger)
    /// are not yet enabled, the only GC trigger is the explicit
    /// `gc_safepoint` at back-edge polling. Callers may safely return
    /// the resulting `RegisterValue` to the bytecode interpreter,
    /// which immediately stores it into a register or pushes it onto
    /// the operand stack — both of which are GC roots once they
    /// receive the value. The window between this call returning and
    /// the caller writing the value into a root is not a safepoint,
    /// so the `GcRef` cannot be reclaimed in that window.
    ///
    /// Once Phase 4 lands, a permanent rootset on `RuntimeState` will
    /// hold every freshly allocated `GcRef<JsStringGc>` until the
    /// next clean safepoint, removing this transient rooting concern.
    pub fn alloc_string_value(
        &mut self,
        value: &str,
    ) -> Result<RegisterValue, InterpreterError> {
        use otter_gc::local::HandleScope;

        let gc_heap = self.objects.gc_heap_mut();
        let mut scope = HandleScope::new(gc_heap);
        let local = crate::js_string_gc::from_str(&mut scope, value)
            .map_err(|_| InterpreterError::OutOfMemory)?;
        let gc_ref = local.as_ref();
        // The scope drops here. The pointer remains valid until the
        // next GC safepoint (see rooting contract above).
        Ok(RegisterValue::from_string_ref(gc_ref))
    }

    /// Allocates a string primitive from raw WTF-16 code units and
    /// returns a tagged [`RegisterValue`]. Same rooting contract as
    /// [`alloc_string_value`]. Preserves lone surrogates verbatim.
    pub fn alloc_string_value_from_utf16(
        &mut self,
        units: &[u16],
    ) -> Result<RegisterValue, InterpreterError> {
        use otter_gc::local::HandleScope;

        let gc_heap = self.objects.gc_heap_mut();
        let mut scope = HandleScope::new(gc_heap);
        let local = crate::js_string_gc::from_utf16_vec(&mut scope, units.to_vec())
            .map_err(|_| InterpreterError::OutOfMemory)?;
        Ok(RegisterValue::from_string_ref(local.as_ref()))
    }

    // -------------------------------------------------------------------------
    // Phase 3: write-barrier stubs.
    //
    // `record_pointer_write` is the single entry point that every site
    // mutating a heap-stored field invokes after the field write. Today
    // (Phase 3 landed, Phase 4 not yet wired) the implementation is a
    // no-op for non-GC values and a remembered-set candidate for
    // GC-managed pointers — recorded so Phase 4 incremental marking
    // and Phase 5 generational scavenger can light up the existing
    // call sites without re-auditing the entire VM.
    //
    // Expected sites:
    //   * `JsObject` property/value mutations (`set_property`,
    //     `set_index`, `define_own_property`, `define_property_storage`).
    //   * Closure / BoundFunction / Promise / Generator / Map / Set /
    //     iterator field mutations.
    //   * Array element pushes / set_indexed_properties_value.
    //
    // The argument shape mirrors the V8 generational barrier:
    //   - `_container_handle` is the slot index of the JsObject being
    //     mutated (legacy TypedHeap address).
    //   - `target_value` is the new field contents — only matters
    //     when it is a heap pointer (`TAG_PTR_STRING`,
    //     `TAG_PTR_OBJECT`, `TAG_PTR_BIGINT`).
    //
    // For Phase 3 we only record the COVERAGE — debug builds verify
    // every store site has a barrier call. Phase 4 wires the actual
    // tri-color shading; Phase 5 wires the remembered set.
    // -------------------------------------------------------------------------

    /// Phase 5: minor (young-generation) GC cycle for the GC-managed
    /// string heap.
    ///
    /// Three-phase pipeline:
    ///
    /// 1. **Root collection** — every `TAG_PTR_STRING` reachable
    ///    from the active register window or embedded inside live
    ///    legacy heap objects gets pushed onto the GcHeap handle
    ///    stack so the scavenger treats it as a root and rewrites
    ///    its slot in place.
    /// 2. **Two-phase scavenge** —
    ///    [`otter_gc::heap::GcHeap::collect_young_no_flip`] runs
    ///    the Cheney copy phase but leaves from-space alive so
    ///    forwarding pointers remain readable.
    /// 3. **External fixup** — every `RegisterValue` slot embedded
    ///    in a live legacy heap object is walked one more time;
    ///    when it carries a `TAG_PTR_STRING` whose target was
    ///    forwarded by the scavenger, the NaN-box bits are
    ///    rewritten in place to point at the new address. Then
    ///    [`otter_gc::heap::GcHeap::flip_after_scavenge_fixup`]
    ///    drops from-space.
    ///
    /// The active register window passed to this call is **not**
    /// fixed up — callers must reload `TAG_PTR_STRING` values from
    /// the register window through the heap's handle-stack roots
    /// (which the scavenger updates in place). The intended caller
    /// is the explicit GC safepoint, where the interpreter knows
    /// which register slots hold heap pointers and can re-read them
    /// after this call returns.
    pub fn gc_collect_strings_minor(&mut self, current_window: &[RegisterValue]) {
        use otter_gc::header::GcHeader;
        use std::collections::HashMap;

        // Step 1: gather roots — register-window TAG_PTR_STRING +
        // every embedded TAG_PTR_STRING in live legacy heap state.
        let mut raw_ptrs: Vec<*const GcHeader> = Vec::new();
        for &rv in current_window {
            if let Some(gc_ref) = rv.as_string_ref() {
                raw_ptrs.push(gc_ref.as_ptr().as_ptr() as *const GcHeader);
            }
        }
        self.objects.scan_register_values(|rv| {
            if let Some(gc_ref) = rv.as_string_ref() {
                raw_ptrs.push(gc_ref.as_ptr().as_ptr() as *const GcHeader);
            }
        });

        // Step 2: push roots, run no-flip scavenge, snapshot the
        // post-scavenge forwarding map. Once we have the map, the
        // GcHeap mutable borrow is released so the fixup pass can
        // grab `&mut self.objects` exclusively.
        let forward_map: HashMap<*const GcHeader, *const GcHeader> = {
            let gc_heap = self.objects.gc_heap_mut();
            let scope = gc_heap.enter_scope();
            for ptr in &raw_ptrs {
                gc_heap.root(*ptr);
            }
            let _result = gc_heap.collect_young_no_flip();
            gc_heap.exit_scope(scope);

            let mut map = HashMap::new();
            gc_heap.walk_forwarded_objects(|old_ptr, new_ptr| {
                map.insert(old_ptr, new_ptr);
            });
            map
        };

        // Step 3: external fixup pass — rewrite every embedded
        // TAG_PTR_STRING whose target moved.
        self.objects
            .fixup_string_refs_after_scavenge(|old_ptr| forward_map.get(&old_ptr).copied());

        // Step 4: drop from-space pages.
        self.objects.gc_heap_mut().flip_after_scavenge_fixup();
    }

    /// Phase 4: full STW GC cycle for the GC-managed string heap
    /// ([`otter_gc::heap::GcHeap`]). Walks every `RegisterValue` reachable
    /// from VM roots — activations + intrinsic registry + every
    /// embedded `RegisterValue` inside live legacy heap objects — and
    /// roots each `TAG_PTR_STRING` reference on the GC's handle stack
    /// before triggering [`otter_gc::heap::GcHeap::collect_full`]. After
    /// the cycle the temporary roots pop off via the saved handle
    /// scope level, so the next allocation starts with a clean stack.
    ///
    /// Safe to call any time; unsafe to *omit* once incremental
    /// marking is wired up. For Phase 4 the trigger is explicit
    /// (called from tests / future safepoints); Phase 5 will hook it
    /// into `poll_back_edge` driven by memory-pressure thresholds.
    pub fn gc_collect_strings_full(&mut self, current_window: &[RegisterValue]) {
        use otter_gc::header::GcHeader;

        // Step 1: gather every RegisterValue we can reach.
        // Step 2: push each TAG_PTR_STRING onto the GcHeap handle
        //   stack so marking treats it as a root.
        // Step 3: call collect_full, then truncate the handle stack
        //   back to its entry level so we don't leak roots.

        // We collect raw pointers first, then push them once we hold
        // `&mut GcHeap`. This avoids re-borrowing `self.objects` while
        // also walking it.
        let mut raw_ptrs: Vec<*const GcHeader> = Vec::new();
        for &rv in current_window {
            if let Some(gc_ref) = rv.as_string_ref() {
                raw_ptrs.push(gc_ref.as_ptr().as_ptr() as *const GcHeader);
            }
        }
        // Walk the legacy heap for every embedded RegisterValue.
        self.objects.scan_register_values(|rv| {
            if let Some(gc_ref) = rv.as_string_ref() {
                raw_ptrs.push(gc_ref.as_ptr().as_ptr() as *const GcHeader);
            }
        });
        // VM-level roots beyond the current register window: the
        // accumulator/secondary_result/closure_handle plus any
        // pending exception are also live. We keep them rooted via
        // the existing legacy collection — strings nested inside
        // them surface through `scan_register_values` because they
        // live in legacy heap objects (Promises, BoundFunctions,
        // ErrorStackFrames, …) that the legacy tracer reaches.

        let gc_heap = self.objects.gc_heap_mut();
        let scope = gc_heap.enter_scope();
        for ptr in &raw_ptrs {
            gc_heap.root(*ptr);
        }
        gc_heap.collect_full();
        gc_heap.exit_scope(scope);
    }

    /// Records a write of `target_value` into a slot owned by the
    /// object behind `container_handle`. Phase 3 stub: no-op for the
    /// common non-GC cases, registers a coverage event in debug.
    #[inline(always)]
    pub fn record_pointer_write(
        &mut self,
        _container_handle: ObjectHandle,
        target_value: RegisterValue,
    ) {
        // Fast path: scalar / inline values carry no GC pointer, so
        // there's nothing to remember.
        let _is_pointer = target_value.is_string_ref()
            || target_value.as_object_handle().is_some()
            || target_value.as_bigint_handle().is_some();
        // Phase 4 hook: when `marking_active`, shade the target gray
        // (Dijkstra insertion barrier). Phase 5 hook: when the
        // container is in old space and the target is in young space,
        // record `&slot` in the remembered set.
        //
        // Both hooks are deliberately deferred — they require the
        // `WriteBarrier` to be active on the underlying `GcHeap`,
        // which is gated on the upcoming incremental-marking
        // (`Phase 4`) and generational-scavenger (`Phase 5`) wiring.
    }

    /// Allocates one BigInt heap value from a [`BigIntPayload`].
    /// (No prototype — BigInt is a primitive type.)
    ///
    /// §6.1.6.2 The BigInt Type
    /// <https://tc39.es/ecma262/#sec-ecmascript-language-types-bigint-type>
    pub fn alloc_bigint(
        &mut self,
        value: crate::bigint_value::BigIntPayload,
    ) -> Result<ObjectHandle, InterpreterError> {
        self.objects
            .alloc_bigint(value)
            .map_err(InterpreterError::from)
    }

    /// Allocates a BigInt by parsing a decimal string. Returns `Err`
    /// if the input is not a valid integer.
    pub fn alloc_bigint_from_str(
        &mut self,
        value: &str,
    ) -> Result<ObjectHandle, InterpreterError> {
        self.objects
            .alloc_bigint_from_str(value)
            .map_err(InterpreterError::from)
    }

    /// Allocates a BigInt from a signed 64-bit integer (always inline).
    pub fn alloc_bigint_from_i64(
        &mut self,
        value: i64,
    ) -> Result<ObjectHandle, InterpreterError> {
        self.objects
            .alloc_bigint_from_i64(value)
            .map_err(InterpreterError::from)
    }

    /// Allocates a fully-initialized RegExp instance with the spec-mandated
    /// own `lastIndex` property.
    ///
    /// §22.2.3.1 RegExpCreate / §22.2.3.1.1 RegExpAlloc steps 4-5 require the
    /// object to expose `lastIndex` as a data property with attributes
    /// `{ [[Writable]]: true, [[Enumerable]]: false, [[Configurable]]: false }`
    /// and value 0. Defining it up front (instead of letting the first write
    /// create a writable/enumerable/configurable slot) is what lets
    /// `/./.lastIndex === 0`, `verifyProperty` checks, and `delete re.lastIndex`
    /// behave per spec.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-regexpcreate>
    pub fn alloc_regexp(
        &mut self,
        pattern: &str,
        flags: &str,
        prototype: Option<ObjectHandle>,
    ) -> Result<ObjectHandle, InterpreterError> {
        let handle = self.objects.alloc_regexp(pattern, flags, prototype)?;
        let last_index = self.intern_property_name("lastIndex");
        let descriptor = crate::object::PropertyValue::data_with_attrs(
            RegisterValue::from_i32(0),
            crate::object::PropertyAttributes::from_flags(true, false, false),
        );
        self.objects
            .define_own_property(handle, last_index, descriptor)
            .ok();
        Ok(handle)
    }

    /// Returns the [`BigIntPayload`] backing a BigInt handle.
    ///
    /// §6.1.6.2 The BigInt Type
    /// <https://tc39.es/ecma262/#sec-ecmascript-language-types-bigint-type>
    pub fn bigint_value(
        &self,
        handle: ObjectHandle,
    ) -> Option<&crate::bigint_value::BigIntPayload> {
        self.objects.bigint_value(handle).ok().flatten()
    }

    /// Allocates one fresh symbol primitive with a VM-wide stable identifier.
    pub fn alloc_symbol(&mut self) -> RegisterValue {
        self.alloc_symbol_with_description(None)
    }

    /// Allocates one fresh symbol primitive and records its optional description.
    pub fn alloc_symbol_with_description(
        &mut self,
        description: Option<Box<str>>,
    ) -> RegisterValue {
        let symbol_id = self.next_symbol_id;
        self.next_symbol_id = self
            .next_symbol_id
            .checked_add(1)
            .expect("symbol identifier space exhausted");
        self.symbol_descriptions.insert(symbol_id, description);
        RegisterValue::from_symbol_id(symbol_id)
    }

    /// Returns the recorded description for a symbol value, if any.
    #[must_use]
    pub fn symbol_description(&self, value: RegisterValue) -> Option<&str> {
        let symbol_id = value.as_symbol_id()?;
        self.symbol_descriptions
            .get(&symbol_id)
            .and_then(|description| description.as_deref())
    }

    /// Interns a global-registry symbol key and returns the canonical symbol value.
    pub fn intern_global_symbol(&mut self, key: Box<str>) -> RegisterValue {
        if let Some(&symbol_id) = self.global_symbol_registry.get(key.as_ref()) {
            return RegisterValue::from_symbol_id(symbol_id);
        }

        let symbol = self.alloc_symbol_with_description(Some(key.clone()));
        let symbol_id = symbol
            .as_symbol_id()
            .expect("allocated symbol should expose a symbol id");
        self.global_symbol_registry.insert(key.clone(), symbol_id);
        self.global_symbol_registry_reverse.insert(symbol_id, key);
        symbol
    }

    /// Returns the registry key for a symbol value, if it was created via `Symbol.for`.
    #[must_use]
    pub fn symbol_registry_key(&self, value: RegisterValue) -> Option<&str> {
        let symbol_id = value.as_symbol_id()?;
        self.global_symbol_registry_reverse
            .get(&symbol_id)
            .map(Box::as_ref)
    }

    /// Allocates a new symbol from a JS-visible description value.
    pub fn create_symbol_from_value(
        &mut self,
        description: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        if description == RegisterValue::undefined() {
            return Ok(self.alloc_symbol_with_description(None));
        }
        let description = self.coerce_symbol_string(description)?;
        Ok(self.alloc_symbol_with_description(Some(description)))
    }

    /// Resolves `Symbol.for(key)` using the runtime-wide global symbol registry.
    pub fn symbol_for_value(
        &mut self,
        key: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        let key = self.coerce_symbol_string(key)?;
        Ok(self.intern_global_symbol(key))
    }

    fn coerce_symbol_string(&mut self, value: RegisterValue) -> Result<Box<str>, InterpreterError> {
        self.js_to_string(value)
    }

    /// Allocates one host-callable function with the runtime default prototype.
    /// The function is bound to the runtime's currently-active realm.
    pub fn alloc_host_function(
        &mut self,
        function: HostFunctionId,
    ) -> Result<ObjectHandle, InterpreterError> {
        let prototype = self.intrinsics().function_prototype();
        let realm = self.current_realm;
        let handle = self.objects.alloc_host_function(function, realm)?;
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("function prototype should exist");
        Ok(handle)
    }

    /// Allocates one host function from descriptor metadata and installs `.name` / `.length`.
    pub fn alloc_host_function_from_descriptor(
        &mut self,
        descriptor: NativeFunctionDescriptor,
    ) -> Result<ObjectHandle, VmNativeCallError> {
        let js_name = descriptor.js_name().to_string();
        let length = descriptor.length();
        let host_function = self.register_native_function(descriptor);
        let handle = self
            .alloc_host_function(host_function)
            .map_err(vm_native_call_error_from_interpreter)?;
        self.install_host_function_length_name(handle, length, &js_name)?;
        Ok(handle)
    }

    /// Installs descriptor-driven members onto one existing host-owned object.
    pub fn install_burrow(
        &mut self,
        target: ObjectHandle,
        descriptors: &[NativeFunctionDescriptor],
    ) -> Result<(), VmNativeCallError> {
        let plan = BurrowBuilder::from_descriptors(descriptors)
            .map(BurrowBuilder::build)
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("failed to normalize host object surface: {error}").into(),
                )
            })?;

        for member in plan.members() {
            match member {
                ObjectMemberPlan::Method(function) => {
                    let host_function = self.register_native_function(function.clone());
                    let handle = self
                        .alloc_host_function(host_function)
                        .map_err(vm_native_call_error_from_interpreter)?;
                    self.install_host_function_length_name(
                        handle,
                        function.length(),
                        function.js_name(),
                    )?;
                    let property = self.intern_property_name(function.js_name());
                    self.objects
                        .define_own_property(
                            target,
                            property,
                            PropertyValue::data_with_attrs(
                                RegisterValue::from_object_handle(handle.0),
                                PropertyAttributes::builtin_method(),
                            ),
                        )
                        .map_err(|error| {
                            VmNativeCallError::Internal(
                                format!(
                                    "failed to install host object method '{}': {error:?}",
                                    function.js_name()
                                )
                                .into(),
                            )
                        })?;
                }
                ObjectMemberPlan::Accessor(accessor) => {
                    let getter = accessor
                        .getter()
                        .cloned()
                        .map(|descriptor| {
                            let function = self.register_native_function(descriptor);
                            self.alloc_host_function(function)
                                .map_err(vm_native_call_error_from_interpreter)
                        })
                        .transpose()?;
                    let setter = accessor
                        .setter()
                        .cloned()
                        .map(|descriptor| {
                            let function = self.register_native_function(descriptor);
                            self.alloc_host_function(function)
                                .map_err(vm_native_call_error_from_interpreter)
                        })
                        .transpose()?;
                    let property = self.intern_property_name(accessor.js_name());
                    self.objects
                        .define_accessor(target, property, getter, setter)
                        .map_err(|error| {
                            VmNativeCallError::Internal(
                                format!(
                                    "failed to install host object accessor '{}': {error:?}",
                                    accessor.js_name()
                                )
                                .into(),
                            )
                        })?;
                }
            }
        }

        Ok(())
    }

    /// Registers a native function and installs it as a property on the global object.
    ///
    /// This is the primary API for embedders to inject host-provided globals
    /// (e.g., `print`, `$DONE`, `$262`) into the runtime.
    pub fn install_native_global(
        &mut self,
        descriptor: crate::descriptors::NativeFunctionDescriptor,
    ) -> Result<ObjectHandle, InterpreterError> {
        let host_fn = self.native_functions.register(descriptor);
        let handle = self.alloc_host_function(host_fn)?;
        let global = self.intrinsics().global_object();
        let prop = self.property_names.intern(
            self.native_functions
                .get(host_fn)
                .expect("just registered")
                .js_name(),
        );
        self.objects
            .set_property(global, prop, RegisterValue::from_object_handle(handle.0))
            .expect("global property installation should succeed");
        Ok(handle)
    }

    /// Installs a value property on the global object.
    pub fn install_global_value(&mut self, name: &str, value: RegisterValue) {
        let global = self.intrinsics().global_object();
        let prop = self.property_names.intern(name);
        self.objects
            .set_property(global, prop, value)
            .expect("global property installation should succeed");
    }

    fn install_host_function_length_name(
        &mut self,
        handle: ObjectHandle,
        length: u16,
        name: &str,
    ) -> Result<(), VmNativeCallError> {
        let length_prop = self.intern_property_name("length");
        self.objects
            .define_own_property(
                handle,
                length_prop,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_i32(i32::from(length)),
                    PropertyAttributes::function_length(),
                ),
            )
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("failed to install function length for '{name}': {error:?}").into(),
                )
            })?;

        let name_prop = self.intern_property_name("name");
        let name_handle = self.alloc_string(name)?;
        self.objects
            .define_own_property(
                handle,
                name_prop,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_object_handle(name_handle.0),
                    PropertyAttributes::function_length(),
                ),
            )
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("failed to install function name for '{name}': {error:?}").into(),
                )
            })?;

        Ok(())
    }

    /// Allocates one bytecode closure with the runtime default function prototype.
    /// The closure is bound to the runtime's currently-active realm.
    pub fn alloc_closure(
        &mut self,
        callee: FunctionIndex,
        upvalues: Vec<ObjectHandle>,
        flags: ObjectClosureFlags,
    ) -> Result<ObjectHandle, InterpreterError> {
        // Generator functions should have %GeneratorFunction.prototype%
        // as their [[Prototype]], not %Function.prototype%.
        let prototype = if flags.is_generator() {
            self.intrinsics().generator_function_prototype()
        } else {
            self.intrinsics().function_prototype()
        };
        let module = self
            .current_module
            .clone()
            .expect("closure allocation requires active module context");
        let realm = self.current_realm;
        let handle = self
            .objects
            .alloc_closure(module, callee, upvalues, flags, realm)?;
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("function prototype should exist");
        let closure_length = self
            .current_module
            .as_ref()
            .and_then(|module| module.function(callee))
            .map(|function| function.length())
            .unwrap_or(0);
        let closure_name = self
            .current_module
            .as_ref()
            .and_then(|module| module.function(callee))
            .and_then(|function| function.name())
            .unwrap_or("")
            .to_string();
        let length_property = self.intern_property_name("length");
        self.objects
            .define_own_property(
                handle,
                length_property,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_i32(i32::from(closure_length)),
                    PropertyAttributes::function_length(),
                ),
            )
            .expect("closure length should install");
        let name_property = self.intern_property_name("name");
        let name_handle = self.alloc_string(closure_name)?;
        self.objects
            .define_own_property(
                handle,
                name_property,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_object_handle(name_handle.0),
                    PropertyAttributes::function_length(),
                ),
            )
            .expect("closure name should install");
        // §10.2.6 MakeConstructor + §27.3.3 — Constructable closures AND
        // generator functions get a `.prototype` own property. Generators
        // are not constructable but still get `.prototype` per §27.3.3.
        if flags.is_constructable() || flags.is_generator() {
            let prototype_property = self.intern_property_name("prototype");
            let constructor_property = self.intern_property_name("constructor");
            let instance_prototype = self.alloc_object()?;
            self.objects
                .define_own_property(
                    handle,
                    prototype_property,
                    PropertyValue::data_with_attrs(
                        RegisterValue::from_object_handle(instance_prototype.0),
                        PropertyAttributes::function_prototype(),
                    ),
                )
                .expect("closure prototype object should install");
            // §27.3.3 — Generator function prototypes do NOT get a
            // `.constructor` back-link. Only regular constructors do.
            if !flags.is_generator() {
                self.objects
                    .define_own_property(
                        instance_prototype,
                        constructor_property,
                        PropertyValue::data_with_attrs(
                            RegisterValue::from_object_handle(handle.0),
                            PropertyAttributes::constructor_link(),
                        ),
                    )
                    .expect("closure prototype.constructor should install");
            }
        }

        Ok(handle)
    }

    /// ES2024 §7.2.1 Type — returns `true` when the value is an ECMAScript
    /// Object (not a primitive). In our VM, strings and BigInts are heap-
    /// allocated but are still primitives per the spec.
    pub fn is_ecma_object(&self, value: RegisterValue) -> bool {
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return false;
        };
        !matches!(
            self.objects.kind(handle),
            Ok(HeapValueKind::String | HeapValueKind::BigInt)
        )
    }

    /// ES2024 §7.2.4 IsConstructor — checks if a value has `[[Construct]]`.
    pub fn is_constructible(&self, handle: ObjectHandle) -> bool {
        match self.objects.kind(handle) {
            Ok(HeapValueKind::HostFunction) => {
                // Host functions are constructors only if registered with Constructor slot kind.
                if let Ok(Some(host_fn_id)) = self.objects.host_function(handle) {
                    self.native_functions.get(host_fn_id).is_some_and(|desc| {
                        desc.slot_kind() == crate::descriptors::NativeSlotKind::Constructor
                    })
                } else {
                    false
                }
            }
            Ok(HeapValueKind::Closure) => self
                .objects
                .closure_flags(handle)
                .is_ok_and(|f| f.is_constructable()),
            Ok(HeapValueKind::BoundFunction) => self
                .objects
                .bound_function_parts(handle)
                .is_ok_and(|(target, _, _)| self.is_constructible(target)),
            Ok(HeapValueKind::Proxy) => {
                // A proxy is constructible if its target is constructible.
                self.objects
                    .proxy_parts(handle)
                    .is_ok_and(|(target, _)| self.is_constructible(target))
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod gc_string_bridge_tests {
    use crate::interpreter::RuntimeState;

    #[test]
    fn alloc_string_value_returns_tagged_string_ref() {
        let mut state = RuntimeState::new();
        let value = state.alloc_string_value("hello").expect("alloc");

        assert!(value.is_string_ref());
        assert!(!value.is_undefined());

        let r = value.as_string_ref().expect("string ref");
        assert_eq!(r.payload().len(), 5);
        // Verify content via the new `js_string_gc` reader.
        assert_eq!(crate::js_string_gc::to_rust_string(r), "hello");
    }

    #[test]
    fn alloc_string_value_is_truthy_unless_empty() {
        let mut state = RuntimeState::new();
        let empty = state.alloc_string_value("").expect("empty");
        let non_empty = state.alloc_string_value("x").expect("non-empty");

        assert!(!empty.is_truthy());
        assert!(non_empty.is_truthy());
    }

    #[test]
    fn alloc_string_value_from_utf16_handles_surrogate_pair() {
        let mut state = RuntimeState::new();
        // U+1F600 "😀" = D83D DE00 surrogate pair.
        let value = state
            .alloc_string_value_from_utf16(&[0xD83D, 0xDE00])
            .expect("alloc");
        let r = value.as_string_ref().expect("string ref");
        assert_eq!(r.payload().len(), 2);
        assert_eq!(crate::js_string_gc::code_unit_at(r, 0), Some(0xD83D));
        assert_eq!(crate::js_string_gc::code_unit_at(r, 1), Some(0xDE00));
        assert_eq!(crate::js_string_gc::code_point_at(r, 0), Some((0x1F600, 2)));
    }

    #[test]
    fn alloc_string_value_round_trips_via_register_value() {
        let mut state = RuntimeState::new();
        // Allocate two strings — they must occupy distinct heap addresses
        // and round-trip independently through RegisterValue tagging.
        let v1 = state.alloc_string_value("alpha").expect("a");
        let v2 = state.alloc_string_value("alpha").expect("b");
        assert!(v1.is_string_ref() && v2.is_string_ref());
        // Different allocations → different pointer payloads → different bits.
        assert_ne!(v1.raw_bits(), v2.raw_bits());
        // But the content compares equal via js_string_gc::equals.
        let r1 = v1.as_string_ref().unwrap();
        let r2 = v2.as_string_ref().unwrap();
        assert!(crate::js_string_gc::equals(r1, r2));
    }

    #[test]
    fn js_to_string_handles_tag_ptr_string() {
        // §7.1.17 ToString on a `TAG_PTR_STRING` value must read the
        // payload directly via the Strategy B reader — not fall through
        // to `[object Object]`.
        let mut state = RuntimeState::new();
        let value = state.alloc_string_value("alpha").expect("alloc");
        let coerced = state.js_to_string(value).expect("coerce");
        assert_eq!(&*coerced, "alpha");
    }

    #[test]
    fn js_to_number_handles_tag_ptr_string() {
        // §7.1.4 ToNumber on a `TAG_PTR_STRING` value parses the string
        // via StringToNumber.
        let mut state = RuntimeState::new();
        let numeric = state.alloc_string_value("42").expect("a");
        let result = state.js_to_number(numeric).expect("coerce");
        assert_eq!(result, 42.0);

        let leading_zero = state.alloc_string_value("0007").expect("b");
        let result = state.js_to_number(leading_zero).expect("coerce");
        assert_eq!(result, 7.0);

        let nan_str = state.alloc_string_value("not-a-number").expect("c");
        let result = state.js_to_number(nan_str).expect("coerce");
        assert!(result.is_nan());

        let empty = state.alloc_string_value("").expect("d");
        let result = state.js_to_number(empty).expect("coerce");
        assert_eq!(result, 0.0);
    }

    #[test]
    fn value_as_string_handles_tag_ptr_string() {
        // §19.2.1 — `eval` and `Function.prototype.bind` use this path
        // to peek a string primitive without coercing.
        let mut state = RuntimeState::new();
        let value = state.alloc_string_value("source-text").expect("alloc");
        let extracted = state.value_as_string(value).expect("string value");
        assert_eq!(extracted, "source-text");

        // Non-string values must still return `None`.
        let number = crate::value::RegisterValue::from_i32(42);
        assert!(state.value_as_string(number).is_none());
    }

    #[test]
    fn gc_collect_strings_full_preserves_rooted_strings() {
        // Phase 4 baseline: a TAG_PTR_STRING value passed in via the
        // `current_window` survives a full GC cycle. Allocate a string,
        // pin it on the register window, GC, verify content readable.
        let mut state = RuntimeState::new();
        let value = state.alloc_string_value("survivor").expect("alloc");
        let window = [value];
        state.gc_collect_strings_full(&window);
        // After GC, the TAG_PTR_STRING is still readable via the new
        // path. If the underlying allocation had been reclaimed we
        // would either crash here or read garbage.
        let gc_ref = value.as_string_ref().expect("string ref");
        assert_eq!(crate::js_string_gc::to_rust_string(gc_ref), "survivor");
    }

    #[test]
    fn gc_collect_strings_minor_preserves_strings_inside_objects() {
        // Phase 5 correctness test: a TAG_PTR_STRING stored in an
        // object property survives a minor GC, and reading the
        // updated property after the cycle yields the new
        // (forwarded) address. The fixup pass rewrites the NaN-box
        // bits in place so subsequent reads do not see a
        // dangling pointer.
        let mut state = RuntimeState::new();
        let str_value = state.alloc_string_value("survivor").expect("alloc");
        let obj = state.alloc_object().expect("obj");
        let prop = state.intern_property_name("data");
        state
            .objects_mut()
            .set_property(obj, prop, str_value)
            .expect("set property");
        // No locals hold the string — only the object property.
        let window: [crate::value::RegisterValue; 0] = [];
        state.gc_collect_strings_minor(&window);
        // Read back the property — should resolve to the new
        // (forwarded) address. Content must match.
        let lookup = state
            .objects()
            .get_property(obj, prop)
            .expect("lookup")
            .expect("present");
        let stored = match lookup.value() {
            crate::object::PropertyValue::Data { value, .. } => value,
            _ => panic!("expected data property"),
        };
        let gc_ref = stored
            .as_string_ref()
            .expect("rooted via fixup pass");
        assert_eq!(crate::js_string_gc::to_rust_string(gc_ref), "survivor");
    }

    #[test]
    fn gc_collect_strings_full_preserves_strings_inside_objects() {
        // A TAG_PTR_STRING stored as an object's property survives GC
        // because `scan_register_values` walks every live HeapValue's
        // embedded `RegisterValue`s and roots them.
        let mut state = RuntimeState::new();
        let str_value = state.alloc_string_value("nested").expect("alloc");
        let obj = state.alloc_object().expect("obj");
        let prop = state.intern_property_name("data");
        state
            .objects_mut()
            .set_property(obj, prop, str_value)
            .expect("set property");
        // Drop the local `str_value` from the window — only the
        // object's property holds it now.
        let window: [crate::value::RegisterValue; 0] = [];
        state.gc_collect_strings_full(&window);
        // The string content is still reachable through the object.
        let lookup = state
            .objects()
            .get_property(obj, prop)
            .expect("lookup")
            .expect("present");
        let stored = match lookup.value() {
            crate::object::PropertyValue::Data { value, .. } => value,
            _ => panic!("expected data property"),
        };
        let gc_ref = stored
            .as_string_ref()
            .expect("rooted via scan_register_values");
        assert_eq!(crate::js_string_gc::to_rust_string(gc_ref), "nested");
    }

    #[test]
    fn coerce_to_string_handle_bridges_to_legacy_handle() {
        // The lazy `+` path in `js_add` and `String.prototype.concat`
        // both call `coerce_to_string_handle`. When given a Strategy B
        // `TAG_PTR_STRING`, the helper materialises a legacy
        // `HeapValue::String` so existing concat / IC / proto chain code
        // keeps working until step 2.8 retires the legacy path.
        let mut state = RuntimeState::new();
        let value = state.alloc_string_value("hello").expect("alloc");
        let legacy_handle =
            state.coerce_to_string_handle(value).expect("legacy bridge");

        // Reading the legacy handle should yield the same content.
        let legacy_str = state
            .objects()
            .js_string_to_rust_string(legacy_handle)
            .expect("legacy read");
        assert_eq!(legacy_str, "hello");
    }
}
