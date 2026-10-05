//! Exact actual-constructor family lookup and rooted publication.
//!
//! # Contents
//! - Per-closure/class family heads and the immediate-function owner registry.
//! - Family replacement on prototype change without retaining old receivers.
//! - Deferred finalization during rooted preparation.
//!
//! # Invariants
//! Base function identity partitions one actual new.target's family list.
//! Prototype replacement creates a new monotonic family identity and detaches
//! the old family from the owner; in-flight canonical tickets keep that old
//! family alive. Closures of the same template and class wrappers never share
//! layout state. Each moving source operand lives in a branded handle across
//! allocations. Generated hit uses only the head family; selecting an existing
//! family moves it to the head without allocation or effects.
//!
//! # See also
//! - `crate::closure_construct` and `crate::class_constructor` for owner heads.
//! - `crate::runtime_state` for the immediate-function registry root walk.

use super::{
    CONSTRUCTOR_PROVISIONAL_CAPACITY, ConstructorFamilyOwner, ConstructorLayout,
    ConstructorLayoutBody,
};
use crate::{
    Interpreter, Value, VmError,
    object::{ShapeHandle, ShapeState},
};

impl Interpreter {
    /// The actual owner kind and function id recorded on a new family.
    fn constructor_family_owner(&self, target: Value) -> (ConstructorFamilyOwner, u32) {
        if let Some(class) = target.as_class_constructor() {
            let ctor = class.ctor(&self.gc_heap);
            let fid = ctor.as_function().or_else(|| {
                ctor.as_closure(&self.gc_heap)
                    .map(|closure| closure.function_id())
            });
            return fid.map_or((ConstructorFamilyOwner::Other, 0), |fid| {
                (ConstructorFamilyOwner::Class, fid)
            });
        }
        if let Some(closure) = target.as_closure(&self.gc_heap) {
            return (ConstructorFamilyOwner::Closure, closure.function_id());
        }
        (ConstructorFamilyOwner::Other, 0)
    }
    fn constructor_layout_head(&self, target: Value) -> Option<ConstructorLayout> {
        if let Some(class) = target.as_class_constructor() {
            return Some(class.constructor_layouts(&self.gc_heap));
        }
        if let Some(closure) = target.as_closure(&self.gc_heap) {
            return Some(closure.constructor_layouts(&self.gc_heap));
        }
        if let Some(bound) = target.as_bound_function() {
            return Some(bound.constructor_layouts(&self.gc_heap));
        }
        if let Some(proxy) = target.as_proxy() {
            return Some(proxy.constructor_layouts(&self.gc_heap));
        }
        if let Some(native) = target.as_native_function() {
            return Some(native.constructor_layouts(&self.gc_heap));
        }
        target.as_function().map(|id| {
            self.function_constructor_layouts
                .get(&id)
                .copied()
                .unwrap_or_else(ConstructorLayout::null)
        })
    }
    fn publish_constructor_layout_head(&mut self, target: Value, head: ConstructorLayout) {
        if let Some(class) = target.as_class_constructor() {
            class.set_constructor_layouts(&mut self.gc_heap, head);
        } else if let Some(closure) = target.as_closure(&self.gc_heap) {
            closure.set_constructor_layouts(&mut self.gc_heap, head);
        } else if let Some(bound) = target.as_bound_function() {
            bound.set_constructor_layouts(&mut self.gc_heap, head);
        } else if let Some(proxy) = target.as_proxy() {
            proxy.set_constructor_layouts(&mut self.gc_heap, head);
        } else if let Some(native) = target.as_native_function() {
            native.set_constructor_layouts(&mut self.gc_heap, head);
        } else {
            let id = target
                .as_function()
                .expect("bytecode constructor layout owner");
            self.function_constructor_layouts.insert(id, head);
        }
    }
    /// Detach one same-base family and move the selected/replacement family
    /// to the head. Layout bodies are old and the whole operation cannot GC.
    fn select_constructor_layout_head(
        &mut self,
        target: Value,
        prior: ConstructorLayout,
        selected: ConstructorLayout,
        base: u32,
    ) {
        let mut previous = ConstructorLayout::null();
        let mut current = prior;
        let mut tail = prior;
        while !current.is_null() {
            let (id, next) = self
                .gc_heap
                .read_payload(current, |body| (body.base_function_id, body.next));
            if id == base {
                if previous.is_null() {
                    tail = next;
                } else {
                    self.gc_heap.with_payload(previous, |body| body.next = next);
                    self.gc_heap.record_write(previous, &next);
                }
                if current != selected {
                    self.gc_heap
                        .with_payload(current, |body| body.detached = true);
                }
                break;
            }
            previous = current;
            current = next;
        }
        self.gc_heap.with_payload(selected, |body| body.next = tail);
        self.gc_heap.record_write(selected, &tail);
        self.publish_constructor_layout_head(target, selected);
    }
    /// OrdinaryCreateFromConstructor already resolved `prototype`. This
    /// operation performs no second lookup and receives the exact effective
    /// new.target after bound/proxy dispatch's required substitutions. Static
    /// field matching runs only on a family miss; existing families already
    /// own their immutable required-slot bound.
    pub(crate) fn constructor_layout_for_receiver(
        &mut self,
        base: u32,
        new_target: Value,
        prototype: Value,
        required_static_slots: impl FnOnce(&Self, Value) -> usize,
    ) -> Result<ConstructorLayout, VmError> {
        self.with_handle_scope(|vm, scope| {
            let target = vm.scoped_value(scope, new_target);
            let prototype = vm.scoped_value(scope, prototype);
            if vm.escape_scoped(target).as_closure(&vm.gc_heap).is_some() {
                let mut target_now = vm.escape_scoped(target);
                vm.ensure_closure_rare(None, &mut target_now, &[])?;
            }
            let target_now = vm.escape_scoped(target);
            let Some(head) = vm.constructor_layout_head(target_now) else {
                return Ok(ConstructorLayout::null());
            };
            let mut current = head;
            let mut selected = ConstructorLayout::null();
            while !current.is_null() {
                let (owner, old_prototype, next) = vm.gc_heap.read_payload(current, |body| {
                    (body.base_function_id, body.prototype, body.next)
                });
                if owner == base {
                    if old_prototype == vm.escape_scoped(prototype) {
                        selected = current;
                    }
                    break;
                }
                current = next;
            }
            if selected.is_null() {
                // Source matching is immutable for this exact base/new.target
                // family. Calculate it only when creating the family, before
                // any shape allocation can move its rooted operands.
                let required_static_slots = required_static_slots(vm, target_now);
                // Register ordinary prototype role/watchpoints through the
                // existing canonical root owner. This generic root is never
                // the constructor family root, and never receives a provisional
                // family root through cache_instance_root.
                let _ = vm.value_root(vm.escape_scoped(prototype), 0, ShapeState::ORDINARY)?;
                let value = vm.escape_scoped(prototype);
                let prototype_kind = match value.as_object() {
                    Some(object) => crate::object::shape_body::ShapePrototype::Object(object),
                    None if value.is_null() => crate::object::shape_body::ShapePrototype::Null,
                    None => crate::object::shape_body::ShapePrototype::Value(value),
                };
                // The handle arena/runtime provider is already published by
                // with_handle_scope. Shapes pin themselves for the runtime turn.
                let mut external = |_visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {};
                let root = vm.shape_runtime.new_root(
                    &mut vm.gc_heap,
                    prototype_kind,
                    usize::from(CONSTRUCTOR_PROVISIONAL_CAPACITY),
                    ShapeHandle::null(),
                    ShapeState::ORDINARY.with_provisional(true),
                    &mut external,
                )?;
                let (owner, owner_function_id) = vm.constructor_family_owner(target_now);
                let body = ConstructorLayoutBody::new(
                    vm.constructor_families.allocate_id(),
                    base,
                    owner,
                    owner_function_id,
                    required_static_slots,
                    vm.escape_scoped(prototype),
                    root,
                    ConstructorLayout::null(),
                );
                selected = vm.gc_heap.alloc_old_with_roots(body, &mut external)?;
                vm.constructor_families.register(&vm.gc_heap, selected);
                vm.gc_heap.record_write(selected, &root);
                // The body's prototype may be young; record its traced edge.
                let prototype_now = vm.escape_scoped(prototype);
                vm.gc_heap.record_write(selected, &prototype_now);
            }
            vm.select_constructor_layout_head(vm.escape_scoped(target), head, selected, base);
            let mut external = |_visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {};
            let _ = super::try_finalize_root(
                selected,
                &mut vm.gc_heap,
                &vm.shape_runtime,
                &mut external,
            );
            Ok(selected)
        })
    }
}
