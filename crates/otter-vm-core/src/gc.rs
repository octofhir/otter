//! Garbage collection support
//!
//! This module provides GC root tracking, tracing interfaces, and a rooting API
//! for safe garbage collection.
//!
//! # Rooting Protocol
//!
//! The rooting API provides safe access to GC-managed objects:
//!
//! - [`Gc<T>`]: An unrooted GC pointer. Only valid within a single operation that
//!   cannot trigger GC. Holding a `Gc<T>` across any function that might allocate
//!   or run JavaScript code is undefined behavior.
//!
//! - [`Handle<T>`]: A rooted GC pointer managed by a [`HandleScope`]. Safe to hold
//!   across function calls that might trigger GC.
//!
//! - [`HandleScope`]: RAII scope that manages handles. All handles created within
//!   a scope are automatically unrooted when the scope drops.
//!
//! # Example
//!
//! ```ignore
//! fn example(ctx: &mut VmContext) {
//!     let scope = HandleScope::new(ctx);
//!
//!     // Root a value to keep it alive across potential GC points
//!     let handle = scope.root_value(Value::int32(42));
//!
//!     // Safe to call functions that might trigger GC
//!     some_js_operation(ctx);
//!
//!     // Handle is still valid
//!     let value = ctx.get_root_slot(handle.slot_index());
//! }  // scope drops, handle is unrooted
//! ```
//!
//! # Safety Invariants
//!
//! 1. `Gc<T>` must not be held across GC points (allocations, JS execution)
//! 2. `Handle<T>` is valid only while its `HandleScope` is alive
//! 3. `HandleScope`s must be dropped in LIFO order (enforced by Rust)
//! 4. Handles should not escape to long-lived storage

use std::any::Any;
use std::cell::Cell;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::Arc;

// Re-export GC types from otter-vm-gc
pub use otter_vm_gc::{
    Allocator as GcAllocator, Collector as GcCollector, GcConfig, GcHeader, GcHeap, GcObject,
    GcStats,
};

/// Trait for types that can be traced by the GC
pub trait Trace {
    /// Trace all references in this object
    fn trace(&self, tracer: &mut dyn Tracer);
}

/// Tracer interface for GC marking phase
pub trait Tracer {
    /// Mark an object as reachable
    fn mark(&mut self, obj: &dyn Any);

    /// Mark a value as reachable
    fn mark_value(&mut self, value: &crate::value::Value);

    /// Mark a GC header as reachable
    fn mark_header(&mut self, header: *const GcHeader);
}

/// A GC root - keeps values alive
pub struct GcRoot<T> {
    value: Arc<T>,
}

impl<T> GcRoot<T> {
    /// Create a new GC root
    pub fn new(value: T) -> Self {
        Self {
            value: Arc::new(value),
        }
    }

    /// Get reference to the value
    pub fn get(&self) -> &T {
        &self.value
    }

    /// Get the Arc
    pub fn arc(&self) -> Arc<T> {
        self.value.clone()
    }
}

impl<T> Clone for GcRoot<T> {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
        }
    }
}

impl<T> std::ops::Deref for GcRoot<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

/// Handle to a GC-managed object
///
/// This is a reference-counted handle that keeps objects alive.
/// The actual GC will use these for root tracking.
pub type GcHandle<T> = Arc<T>;

/// Create a new GC-managed handle
pub fn gc_alloc<T>(value: T) -> GcHandle<T> {
    Arc::new(value)
}

/// Raw handle to a GC pointer (low-level, unsafe)
///
/// This is a thin wrapper around a raw pointer for internal use.
/// For safe rooted handles, use [`Handle<T>`] with [`HandleScope`].
pub struct RawHandle<T> {
    ptr: *const T,
    _marker: PhantomData<T>,
}

impl<T> RawHandle<T> {
    /// Create a new raw handle
    ///
    /// # Safety
    /// The pointer must be valid and point to a live object.
    pub unsafe fn new(ptr: *const T) -> Self {
        Self {
            ptr,
            _marker: PhantomData,
        }
    }

    /// Get reference to underlying object
    ///
    /// # Safety
    /// The pointer must still be valid.
    pub unsafe fn get(&self) -> &T {
        unsafe { &*self.ptr }
    }

    /// Get mutable reference
    ///
    /// # Safety
    /// Must have exclusive access.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_mut(&self) -> &mut T {
        unsafe { &mut *(self.ptr as *mut T) }
    }

    /// Get raw pointer
    pub fn as_ptr(&self) -> *const T {
        self.ptr
    }
}

// RawHandle is Send if T is Send
unsafe impl<T: Send> Send for RawHandle<T> {}
// RawHandle is Sync if T is Sync
unsafe impl<T: Sync> Sync for RawHandle<T> {}

// ─────────────────────────────────────────────────────────────────────────────
// Rooting API: GcBox, Gc, Handle, HandleScope
// ─────────────────────────────────────────────────────────────────────────────

/// A GC-managed heap cell containing a value and GC metadata.
///
/// This is the low-level allocation unit for GC objects. The header
/// contains marking bits for tri-color marking.
#[repr(C)]
pub struct GcBox<T> {
    /// GC metadata (mark bits, generation, etc.)
    header: GcHeader,
    /// The actual value
    value: T,
}

impl<T> GcBox<T> {
    /// Create a new GcBox with the given value.
    ///
    /// Note: This creates an unallocated GcBox. Use `Gc::alloc` to allocate
    /// on the GC heap.
    pub fn new(value: T) -> Self {
        Self {
            header: GcHeader::new(0), // Type tag 0 for now
            value,
        }
    }

    /// Get reference to the header
    pub fn header(&self) -> &GcHeader {
        &self.header
    }

    /// Get reference to the value
    pub fn value(&self) -> &T {
        &self.value
    }

    /// Get mutable reference to the value
    pub fn value_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

/// An unrooted GC pointer.
///
/// `Gc<T>` is a thin wrapper around a pointer to a GC-managed object.
/// It is only valid within a single operation that cannot trigger GC.
///
/// # Safety
///
/// Holding a `Gc<T>` across any function that might:
/// - Allocate memory
/// - Run JavaScript code
/// - Trigger garbage collection
///
/// ...is undefined behavior, as the pointer may become dangling.
///
/// To keep a value alive across GC points, root it using [`HandleScope::root`].
#[derive(Debug)]
pub struct Gc<T> {
    ptr: NonNull<GcBox<T>>,
}

impl<T> Gc<T> {
    /// Create from a raw NonNull pointer.
    ///
    /// # Safety
    /// The pointer must be valid and point to a live GcBox<T>.
    pub unsafe fn from_raw(ptr: NonNull<GcBox<T>>) -> Self {
        Self { ptr }
    }

    /// Get the raw pointer.
    pub fn as_ptr(&self) -> *const GcBox<T> {
        self.ptr.as_ptr()
    }

    /// Get reference to the inner value.
    ///
    /// # Safety
    /// The GcBox must still be live (not collected).
    pub unsafe fn get(&self) -> &T {
        // SAFETY: Caller guarantees the GcBox is still live
        unsafe { &(*self.ptr.as_ptr()).value }
    }

    /// Get mutable reference to the inner value.
    ///
    /// # Safety
    /// Caller must ensure exclusive access and that GcBox is still live.
    pub unsafe fn get_mut(&mut self) -> &mut T {
        // SAFETY: Caller guarantees exclusive access and GcBox is still live
        unsafe { &mut (*self.ptr.as_ptr()).value }
    }

    /// Get the GC header.
    pub fn header(&self) -> &GcHeader {
        unsafe { &(*self.ptr.as_ptr()).header }
    }
}

// Gc<T> is Copy for ergonomic use within a single operation
impl<T> Clone for Gc<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Gc<T> {}

// Do NOT implement Send/Sync - Gc<T> should not escape the current operation

// ─────────────────────────────────────────────────────────────────────────────
// GcRef<T> - Safe reference to GC-managed object (replaces Arc<T>)
// ─────────────────────────────────────────────────────────────────────────────

/// A safe reference to a GC-managed object.
///
/// `GcRef<T>` is the primary way to hold references to GC-managed objects.
/// It replaces `Arc<T>` in the codebase and provides safe access through `Deref`.
///
/// # Safety Model
///
/// `GcRef<T>` is safe to use because:
/// 1. It can only be created from a valid `Gc<T>` via `GcRef::from_gc`
/// 2. Any `GcRef<T>` stored in a `Value` is traced by the GC
/// 3. Objects reachable from roots survive garbage collection
///
/// The GC ensures that any `GcRef<T>` that's reachable (stored in registers,
/// locals, object properties, etc.) points to a live object.
///
/// # Thread Safety
///
/// `GcRef<T>` is `Send + Sync` when `T` is `Send + Sync`, matching `Arc<T>`
/// behavior. The underlying `GcBox<T>` uses interior mutability (RwLock)
/// for thread-safe access where needed.
#[derive(Debug)]
pub struct GcRef<T> {
    gc: Gc<T>,
}

impl<T> GcRef<T> {
    /// Create a new GcRef from a Gc pointer.
    ///
    /// # Safety
    /// The Gc pointer must point to a valid, live GcBox<T>.
    /// The caller must ensure the object will remain rooted/reachable.
    pub unsafe fn from_gc(gc: Gc<T>) -> Self {
        Self { gc }
    }

    /// Create a GcRef by allocating a new GcBox on the heap.
    ///
    /// This registers the allocation with the global GC registry.
    /// The returned GcRef owns the allocation.
    pub fn new(value: T) -> Self
    where
        T: otter_vm_gc::GcTraceable + 'static,
    {
        // Allocate and register with GC
        // gc_alloc returns *mut T (pointer to value after header)
        let value_ptr = unsafe { otter_vm_gc::gc_alloc(value) };

        // Calculate pointer to GcBox<T> (which starts at the header)
        // GcBox is #[repr(C)] with header first, then value
        // So: GcBox address = value address - offset_of(value in GcBox)
        let box_ptr = unsafe {
            let offset = std::mem::offset_of!(GcBox<T>, value);
            (value_ptr as *mut u8).sub(offset) as *mut GcBox<T>
        };

        let non_null = NonNull::new(box_ptr).unwrap();

        Self {
            gc: unsafe { Gc::from_raw(non_null) },
        }
    }

    /// Get the underlying Gc pointer.
    pub fn as_gc(&self) -> Gc<T> {
        self.gc
    }

    /// Get the GC header.
    pub fn header(&self) -> &GcHeader {
        self.gc.header()
    }

    /// Get raw pointer (for identity comparison).
    pub fn as_ptr(&self) -> *const T {
        unsafe { &(*self.gc.as_ptr()).value as *const T }
    }
}

impl<T> std::ops::Deref for GcRef<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: GcRef can only exist for live objects that are rooted/reachable.
        // The GC guarantees objects won't be collected while reachable.
        unsafe { self.gc.get() }
    }
}

impl<T> Clone for GcRef<T> {
    fn clone(&self) -> Self {
        Self { gc: self.gc }
    }
}

// GcRef is Copy like Gc - it's just a pointer
impl<T> Copy for GcRef<T> {}

// GcRef is Send + Sync when T is, matching Arc behavior
unsafe impl<T: Send + Sync> Send for GcRef<T> {}
unsafe impl<T: Send + Sync> Sync for GcRef<T> {}

impl<T: PartialEq> PartialEq for GcRef<T> {
    fn eq(&self, other: &Self) -> bool {
        // Compare by pointer identity first (fast path)
        if self.as_ptr() == other.as_ptr() {
            return true;
        }
        // Fall back to value comparison
        **self == **other
    }
}

impl<T: Eq> Eq for GcRef<T> {}

impl<T: std::hash::Hash> std::hash::Hash for GcRef<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state)
    }
}

impl<T: std::fmt::Display> std::fmt::Display for GcRef<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (**self).fmt(f)
    }
}

impl<T> AsRef<T> for GcRef<T> {
    fn as_ref(&self) -> &T {
        self
    }
}

/// A rooted handle to a GC-managed object.
///
/// `Handle<T>` keeps its target alive across garbage collection cycles.
/// Handles are created by [`HandleScope::root`] and are automatically
/// unrooted when their scope drops.
///
/// Unlike [`Gc<T>`], `Handle<T>` is safe to hold across function calls
/// that might trigger GC.
#[derive(Debug)]
pub struct Handle<T> {
    /// Index into VmContext's root_slots
    slot_index: usize,
    _marker: PhantomData<T>,
}

impl<T> Handle<T> {
    /// Create a new handle with the given slot index.
    ///
    /// This is internal - use [`HandleScope::root`] to create handles.
    fn new(slot_index: usize) -> Self {
        Self {
            slot_index,
            _marker: PhantomData,
        }
    }

    /// Get the slot index (for internal use and testing).
    pub fn slot_index(&self) -> usize {
        self.slot_index
    }

    /// Check if the handle is valid (has a valid slot index).
    pub fn is_valid(&self) -> bool {
        // A handle is valid if it has been created by a HandleScope
        // The actual validity depends on the scope still being alive
        true // Slot index is always valid within its scope
    }
}

// Handle is Copy for ergonomic use
impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Handle<T> {}

/// RAII scope for managing rooted handles.
///
/// All handles created within a scope are automatically unrooted
/// when the scope drops. Scopes can be nested, and inner scopes
/// must drop before outer scopes (enforced by Rust's borrow checker).
///
/// # Example
///
/// ```ignore
/// fn example(ctx: &mut VmContext) {
///     let scope = HandleScope::new(ctx);
///
///     let handle = scope.root_value(Value::int32(42));
///     assert_eq!(ctx.root_count(), 1);
///
///     call_js_function(ctx);  // May trigger GC
///
///     // handle is still valid!
///     let value = ctx.get_root_slot(handle.slot_index());
/// }  // scope drops, handle unrooted, ctx.root_count() == 0
/// ```
pub struct HandleScope<'ctx> {
    /// Pointer to VmContext (we need interior mutability for root operations)
    ctx: *mut crate::context::VmContext,
    /// Index of first slot owned by this scope (for future escape functionality)
    #[allow(dead_code)]
    base_index: usize,
    /// Number of slots allocated by this scope
    slot_count: Cell<usize>,
    /// Lifetime marker
    _marker: PhantomData<&'ctx mut crate::context::VmContext>,
}

impl<'ctx> HandleScope<'ctx> {
    /// Create a new handle scope.
    ///
    /// The scope borrows the VmContext mutably for its lifetime.
    pub fn new(ctx: &'ctx mut crate::context::VmContext) -> Self {
        let base_index = ctx.root_slots_len();
        ctx.push_scope_marker(base_index);

        Self {
            ctx: ctx as *mut _,
            base_index,
            slot_count: Cell::new(0),
            _marker: PhantomData,
        }
    }

    /// Root a Value, returning a Handle.
    ///
    /// The handle will keep the value alive until this scope drops.
    pub fn root_value(&self, value: crate::value::Value) -> Handle<crate::value::Value> {
        let index = self.allocate_slot(value);
        Handle::new(index)
    }

    /// Get reference to the VmContext.
    pub fn context(&self) -> &crate::context::VmContext {
        unsafe { &*self.ctx }
    }

    /// Get mutable reference to the VmContext.
    ///
    /// This uses interior mutability through a raw pointer because HandleScope
    /// needs to modify root_slots while allowing immutable access for Handle::get.
    #[allow(clippy::mut_from_ref)]
    pub fn context_mut(&self) -> &mut crate::context::VmContext {
        unsafe { &mut *self.ctx }
    }

    /// Allocate a slot for a value and return its index.
    fn allocate_slot(&self, value: crate::value::Value) -> usize {
        let ctx = unsafe { &mut *self.ctx };
        let index = ctx.push_root_slot(value);
        self.slot_count.set(self.slot_count.get() + 1);
        index
    }
}

impl<'ctx> Drop for HandleScope<'ctx> {
    fn drop(&mut self) {
        let ctx = unsafe { &mut *self.ctx };
        ctx.pop_root_slots(self.slot_count.get());
        ctx.pop_scope_marker();
    }
}

// Implement Trace for Value
impl Trace for crate::value::Value {
    fn trace(&self, tracer: &mut dyn Tracer) {
        tracer.mark_value(self);
    }
}

// Implement Trace for JsObject
impl Trace for crate::object::JsObject {
    fn trace(&self, tracer: &mut dyn Tracer) {
        // Trace the current shape
        tracer.mark(self.shape().as_ref());

        use crate::object::PropertyDescriptor;

        // Trace inline property values (first INLINE_PROPERTY_COUNT)
        {
            let inline = self.get_inline_properties_storage();
            let inline_props = inline.read();
            for slot in inline_props.iter() {
                if let Some(entry) = slot {
                    match &entry.desc {
                        PropertyDescriptor::Data { value, .. } => {
                            tracer.mark_value(value);
                        }
                        PropertyDescriptor::Accessor { get, set, .. } => {
                            if let Some(v) = get {
                                tracer.mark_value(v);
                            }
                            if let Some(v) = set {
                                tracer.mark_value(v);
                            }
                        }
                        PropertyDescriptor::Deleted => {}
                    }
                }
            }
        }

        // Trace overflow property values (Data or Accessor)
        {
            let overflow = self.get_overflow_properties_storage();
            let overflow_props = overflow.read();
            for entry in overflow_props.iter() {
                match &entry.desc {
                    PropertyDescriptor::Data { value, .. } => {
                        tracer.mark_value(value);
                    }
                    PropertyDescriptor::Accessor { get, set, .. } => {
                        if let Some(v) = get {
                            tracer.mark_value(v);
                        }
                        if let Some(v) = set {
                            tracer.mark_value(v);
                        }
                    }
                    PropertyDescriptor::Deleted => {}
                }
            }
        }

        // Trace indexed elements
        {
            let elements = self.get_elements_storage();
            let elems = elements.read();
            for value in elems.iter() {
                tracer.mark_value(value);
            }
        }

        // Trace prototype
        if let Some(proto) = self.prototype() {
            tracer.mark(proto.as_ref());
        }
    }
}

// Implement Trace for Shape
impl Trace for crate::shape::Shape {
    fn trace(&self, tracer: &mut dyn Tracer) {
        self.trace(tracer);
    }
}

// Implement Trace for Closure
impl Trace for crate::value::Closure {
    fn trace(&self, tracer: &mut dyn Tracer) {
        // Trace captured upvalues
        for cell in &self.upvalues {
            cell.trace(tracer);
        }

        // Trace the associated function object
        tracer.mark(self.object.as_ref());
    }
}

// Implement Trace for GeneratorFrame
impl Trace for crate::generator::GeneratorFrame {
    fn trace(&self, tracer: &mut dyn Tracer) {
        for val in &self.locals {
            tracer.mark_value(val);
        }
        for val in &self.registers {
            tracer.mark_value(val);
        }
        for cell in &self.upvalues {
            cell.trace(tracer);
        }
        tracer.mark_value(&self.this_value);
        if let Some(val) = &self.received_value {
            tracer.mark_value(val);
        }
        if let Some(val) = &self.pending_throw {
            tracer.mark_value(val);
        }
    }
}

// Implement Trace for JsGenerator
impl Trace for crate::generator::JsGenerator {
    fn trace(&self, tracer: &mut dyn Tracer) {
        // Trace the associated object
        tracer.mark(self.object.as_ref());

        // Trace upvalues
        for cell in &self.upvalues {
            cell.trace(tracer);
        }

        // Trace initial arguments and this
        for val in &*self.initial_args.lock() {
            tracer.mark_value(val);
        }
        tracer.mark_value(&*self.initial_this.lock());

        // Trace abrupt return/throw
        if let Some(val) = &*self.abrupt_return.lock() {
            tracer.mark_value(val);
        }
        if let Some(val) = &*self.abrupt_throw.lock() {
            tracer.mark_value(val);
        }

        // Trace saved frame
        if let Some(frame) = &*self.frame.lock() {
            frame.trace(tracer);
        }
    }
}

// Implement Trace for UpvalueCell
impl Trace for crate::value::UpvalueCell {
    fn trace(&self, tracer: &mut dyn Tracer) {
        tracer.mark_value(&self.get());
    }
}

// Implement Trace for CallFrame
impl Trace for crate::context::CallFrame {
    fn trace(&self, tracer: &mut dyn Tracer) {
        // Trace locals
        for value in &self.locals {
            tracer.mark_value(value);
        }

        // Trace captured upvalues
        for cell in &self.upvalues {
            cell.trace(tracer);
        }

        // Trace the `this` value
        tracer.mark_value(&self.this_value);

        // Trace home_object (for super resolution)
        if let Some(ref home) = self.home_object {
            tracer.mark_header(home.header() as *const GcHeader);
        }
        // Trace new_target_proto (for multi-level inheritance)
        if let Some(ref ntp) = self.new_target_proto {
            tracer.mark_header(ntp.header() as *const GcHeader);
        }
    }
}

// Implement Trace for VmContext
impl Trace for crate::context::VmContext {
    fn trace(&self, tracer: &mut dyn Tracer) {
        // Trace global object
        tracer.mark(self.global().as_ref());

        // Trace registers
        for value in self.registers_to_trace().iter() {
            tracer.mark_value(value);
        }

        // Trace call stack
        for frame in self.call_stack().iter() {
            frame.trace(tracer);
        }

        // Trace exception if any
        if let Some(exc) = self.exception() {
            tracer.mark_value(exc);
        }

        // Trace pending call state
        for arg in self.pending_args_to_trace().iter() {
            tracer.mark_value(arg);
        }

        if let Some(this) = self.pending_this_to_trace() {
            tracer.mark_value(this);
        }

        for cell in self.pending_upvalues_to_trace().iter() {
            cell.trace(tracer);
        }

        // Trace open upvalues
        for cell in self.open_upvalues_to_trace().values() {
            cell.trace(tracer);
        }

        // Trace root slots (handles managed by HandleScope)
        for value in self.root_slots_to_trace() {
            tracer.mark_value(value);
        }
    }
}

// Implement Trace for SavedFrame
impl Trace for crate::async_context::SavedFrame {
    fn trace(&self, tracer: &mut dyn Tracer) {
        for val in &self.locals {
            tracer.mark_value(val);
        }
        for val in &self.registers {
            tracer.mark_value(val);
        }
        for cell in &self.upvalues {
            cell.trace(tracer);
        }
        tracer.mark_value(&self.this_value);
    }
}

// Implement Trace for AsyncContext
impl Trace for crate::async_context::AsyncContext {
    fn trace(&self, tracer: &mut dyn Tracer) {
        for frame in &self.frames {
            frame.trace(tracer);
        }
        tracer.mark(self.result_promise.as_ref());
        tracer.mark(self.awaited_promise.as_ref());
    }
}

// Implement Trace for InlineCacheState
impl Trace for otter_vm_bytecode::function::InlineCacheState {
    fn trace(&self, _tracer: &mut dyn Tracer) {
        // Caches stores raw pointers (u64) and offsets.
        // We don't trace them here as the transition tree and objects
        // keep the Shapes alive.
    }
}

// Implement Trace for bytecode Function (to mark feedback vector)
impl Trace for otter_vm_bytecode::function::Function {
    fn trace(&self, tracer: &mut dyn Tracer) {
        let feedback = self.feedback_vector.read();
        for ic in feedback.iter() {
            ic.ic_state.trace(tracer);
        }
    }
}

// Implement Trace for JsProxy
impl Trace for crate::proxy::JsProxy {
    fn trace(&self, tracer: &mut dyn Tracer) {
        tracer.mark(self.target.as_ref());
        tracer.mark(self.handler.as_ref());
    }
}

// Implement Trace for JsPromise
impl Trace for crate::promise::JsPromise {
    fn trace(&self, tracer: &mut dyn Tracer) {
        // Trace state
        let state = self.state.lock();
        match &*state {
            crate::promise::PromiseState::Pending => {}
            crate::promise::PromiseState::Fulfilled(value) => tracer.mark_value(value),
            crate::promise::PromiseState::Rejected(value) => tracer.mark_value(value),
        }
        // Note: we cannot trace callbacks in Box<dyn Fn> as they are opaque.
        // This is a known limitation.
    }
}

// Implement Trace for JsArrayBuffer
impl Trace for crate::array_buffer::JsArrayBuffer {
    fn trace(&self, _tracer: &mut dyn Tracer) {
        // Raw data, no pointers
    }
}

// Implement Trace for SharedArrayBuffer
impl Trace for crate::shared_buffer::SharedArrayBuffer {
    fn trace(&self, _tracer: &mut dyn Tracer) {
        // Raw data, no pointers
    }
}

// Implement Trace for Symbol
impl Trace for crate::value::Symbol {
    fn trace(&self, _tracer: &mut dyn Tracer) {
        // Only string description, no pointers
    }
}

// Implement Trace for BigInt
impl Trace for crate::value::BigInt {
    fn trace(&self, _tracer: &mut dyn Tracer) {
        // Only string value, no pointers
    }
}

// Implement Trace for JsTypedArray
impl Trace for crate::typed_array::JsTypedArray {
    fn trace(&self, tracer: &mut dyn Tracer) {
        self.buffer().trace(tracer);
    }
}

// Implement Trace for JsDataView
impl Trace for crate::data_view::JsDataView {
    fn trace(&self, tracer: &mut dyn Tracer) {
        self.buffer().trace(tracer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gc_root() {
        let root = GcRoot::new(42);
        assert_eq!(*root.get(), 42);
    }

    #[test]
    fn test_gc_handle() {
        let handle = gc_alloc("hello".to_string());
        assert_eq!(handle.as_str(), "hello");
    }

    #[test]
    fn test_gc_heap_integration() {
        let heap = GcHeap::new();
        assert_eq!(heap.allocated(), 0);

        let ptr = heap.allocate_old(100);
        assert!(ptr.is_some());
    }

    #[test]
    fn test_gc_collector_integration() {
        let heap = GcHeap::new();
        let mut collector = GcCollector::new(heap);
        collector.collect(&[]);
        assert_eq!(collector.stats().collections, 1);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Rooting API tests
    // ─────────────────────────────────────────────────────────────────────────

    fn create_test_context() -> crate::context::VmContext {
        let memory_manager = Arc::new(crate::memory::MemoryManager::new(1024 * 1024 * 100));
        let global = GcRef::new(crate::object::JsObject::new(None, memory_manager.clone()));
        crate::context::VmContext::new(global, memory_manager)
    }

    #[test]
    fn test_handle_scope_basic() {
        let mut ctx = create_test_context();
        let initial_count = ctx.root_count();

        {
            let scope = HandleScope::new(&mut ctx);

            let handle = scope.root_value(crate::value::Value::int32(42));
            assert!(handle.is_valid());
            assert_eq!(scope.context().root_count(), initial_count + 1);

            // Value should be accessible
            let value = scope.context().get_root_slot(handle.slot_index());
            assert_eq!(value.as_int32(), Some(42));
        }

        // After scope drop, roots should be cleaned
        assert_eq!(ctx.root_count(), initial_count);
    }

    #[test]
    fn test_handle_scope_drop_cleans_roots() {
        let mut ctx = create_test_context();
        let initial_root_count = ctx.root_count();

        {
            let scope = HandleScope::new(&mut ctx);
            let _handle1 = scope.root_value(crate::value::Value::int32(1));
            let _handle2 = scope.root_value(crate::value::Value::int32(2));
            let _handle3 = scope.root_value(crate::value::Value::int32(3));
            assert_eq!(scope.context().root_count(), initial_root_count + 3);
        }

        // After scope drop, all roots should be cleaned
        assert_eq!(ctx.root_count(), initial_root_count);
    }

    #[test]
    fn test_nested_handle_scopes() {
        let mut ctx = create_test_context();
        let initial = ctx.root_count();

        {
            let outer = HandleScope::new(&mut ctx);
            let h1 = outer.root_value(crate::value::Value::int32(1));
            assert_eq!(outer.context().root_count(), initial + 1);

            {
                let inner = HandleScope::new(outer.context_mut());
                let _h2 = inner.root_value(crate::value::Value::int32(2));
                assert_eq!(inner.context().root_count(), initial + 2);
            }

            // Inner scope dropped, but outer handle still valid
            assert_eq!(outer.context().root_count(), initial + 1);
            let value = outer.context().get_root_slot(h1.slot_index());
            assert_eq!(value.as_int32(), Some(1));
        }

        // Both scopes dropped
        assert_eq!(ctx.root_count(), initial);
    }

    #[test]
    fn test_gcbox_basic() {
        let gcbox = GcBox::new(42i32);
        assert_eq!(*gcbox.value(), 42);
    }

    #[test]
    fn test_gc_pointer_basic() {
        let mut gcbox = Box::new(GcBox::new(100i32));
        let ptr = NonNull::new(gcbox.as_mut()).unwrap();

        let gc = unsafe { Gc::from_raw(ptr) };
        assert_eq!(unsafe { *gc.get() }, 100);

        // Gc is Copy
        let gc2 = gc;
        assert_eq!(unsafe { *gc2.get() }, 100);
    }

    #[test]
    fn test_handle_copy() {
        let mut ctx = create_test_context();

        let scope = HandleScope::new(&mut ctx);
        let handle = scope.root_value(crate::value::Value::int32(42));

        // Handle is Copy
        let handle2 = handle;
        assert_eq!(handle.slot_index(), handle2.slot_index());
    }
}
