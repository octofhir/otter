//! Handle-based rooting for GC objects.
//!
//! Like V8's `HandleScope` / `Local<T>` / `Global<T>`:
//!
//! - **`HandleStack`**: A growable vector of `*const GcHeader` pointers.
//!   The GC treats all entries as roots during collection.
//! - **`HandleScope`**: RAII guard that saves the handle stack top on creation
//!   and truncates back to it on drop. Scopes nest naturally.
//! - **`LocalHandle`**: An index into the handle stack, valid within the
//!   enclosing `HandleScope`. Lightweight (just a u32 index).
//! - **`GlobalHandle`**: A separately tracked root that outlives any scope.
//!   Must be explicitly dropped.
//!
//! # Safety model
//!
//! The handle stack is owned by `GcHeap`. All handle operations go through
//! `&mut GcHeap`, which Rust's borrow checker ensures is exclusive.
//! `LocalHandle` is `Copy` — it's just an index, no reference counting.
//! Dangling handles (accessing a handle after its scope has been dropped)
//! are caught by bounds checking in debug mode.

use crate::header::GcHeader;

/// The handle stack — a flat array of rooted GC pointers.
///
/// The GC scans this array during collection to discover root objects.
/// Handles are added via `push` and bulk-removed via `truncate` (when
/// a HandleScope drops).
pub struct HandleStack {
    /// Raw GC pointers that are currently rooted.
    entries: Vec<*const GcHeader>,
}

impl HandleStack {
    pub fn new() -> Self {
        Self {
            entries: Vec::with_capacity(64),
        }
    }

    /// Pushes a GC pointer onto the handle stack, returning its index.
    #[inline]
    pub fn push(&mut self, ptr: *const GcHeader) -> u32 {
        let index = self.entries.len() as u32;
        self.entries.push(ptr);
        index
    }

    /// Returns the current stack top (for HandleScope save/restore).
    #[inline]
    pub fn top(&self) -> u32 {
        self.entries.len() as u32
    }

    /// Truncates the stack to the given level (HandleScope drop).
    #[inline]
    pub fn truncate(&mut self, level: u32) {
        self.entries.truncate(level as usize);
    }

    /// Reads the pointer at the given index.
    #[inline]
    pub fn get(&self, index: u32) -> Option<*const GcHeader> {
        self.entries.get(index as usize).copied()
    }

    /// Updates the pointer at the given index (used by scavenger to update
    /// moved objects).
    #[inline]
    pub fn set(&mut self, index: u32, ptr: *const GcHeader) {
        if let Some(slot) = self.entries.get_mut(index as usize) {
            *slot = ptr;
        }
    }

    /// Returns all entries as root slots for the GC marker.
    /// Each entry is a pointer-to-pointer that the scavenger/compactor can update.
    pub fn root_slots(&mut self) -> Vec<*mut *const GcHeader> {
        self.entries
            .iter_mut()
            .map(|slot| slot as *mut *const GcHeader)
            .collect()
    }

    /// Returns all entries as immutable root pointers for the marker.
    pub fn root_pointers(&self) -> &[*const GcHeader] {
        &self.entries
    }

    /// Number of entries on the stack.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the stack is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for HandleStack {
    fn default() -> Self {
        Self::new()
    }
}

// Safe to send between threads when transferring isolate ownership.
unsafe impl Send for HandleStack {}

/// A rooted handle index, valid within the enclosing `HandleScope`.
///
/// `Copy` and cheap (4 bytes). Does not prevent GC — it merely marks the
/// referenced object as a root while the enclosing scope is alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LocalHandle(u32);

impl LocalHandle {
    /// Creates a handle from a raw index. Typically only called by HandleStack.
    pub const fn from_raw(index: u32) -> Self {
        Self(index)
    }

    /// Returns the raw index into the handle stack.
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// A long-lived rooted handle that outlives any HandleScope.
///
/// Must be explicitly released via [`GlobalHandleTable::release`].
/// While alive, the referenced object is a GC root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlobalHandle(u32);

impl GlobalHandle {
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// Table of global (persistent) handles.
///
/// Separate from the HandleStack because global handles are not scoped —
/// they persist until explicitly released.
pub struct GlobalHandleTable {
    /// Active global handles. `None` entries are free slots.
    entries: Vec<Option<*const GcHeader>>,
    /// Free list of released slot indices.
    free_list: Vec<u32>,
}

impl GlobalHandleTable {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            free_list: Vec::new(),
        }
    }

    /// Creates a new global handle rooting the given object.
    pub fn create(&mut self, ptr: *const GcHeader) -> GlobalHandle {
        let index = if let Some(free_index) = self.free_list.pop() {
            self.entries[free_index as usize] = Some(ptr);
            free_index
        } else {
            let index = self.entries.len() as u32;
            self.entries.push(Some(ptr));
            index
        };
        GlobalHandle(index)
    }

    /// Releases a global handle, allowing the referenced object to be collected.
    pub fn release(&mut self, handle: GlobalHandle) {
        if let Some(slot) = self.entries.get_mut(handle.0 as usize) {
            *slot = None;
            self.free_list.push(handle.0);
        }
    }

    /// Reads the pointer for a global handle.
    pub fn get(&self, handle: GlobalHandle) -> Option<*const GcHeader> {
        self.entries.get(handle.0 as usize).and_then(|opt| *opt)
    }

    /// Updates the pointer for a global handle (scavenger/compactor moved the object).
    pub fn set(&mut self, handle: GlobalHandle, ptr: *const GcHeader) {
        if let Some(slot) = self.entries.get_mut(handle.0 as usize) {
            *slot = Some(ptr);
        }
    }

    /// Returns all active global handles as root slots for GC.
    pub fn root_slots(&mut self) -> Vec<*mut *const GcHeader> {
        self.entries
            .iter_mut()
            .filter_map(|slot| slot.as_mut().map(|ptr| ptr as *mut *const GcHeader))
            .collect()
    }

    /// Number of active (non-released) global handles.
    pub fn active_count(&self) -> usize {
        self.entries.iter().filter(|e| e.is_some()).count()
    }
}

impl Default for GlobalHandleTable {
    fn default() -> Self {
        Self::new()
    }
}

unsafe impl Send for GlobalHandleTable {}

/// RAII handle scope that saves and restores the handle stack level.
///
/// All `LocalHandle`s created within a scope become invalid when the scope
/// drops. Scopes nest: an inner scope's drop only releases handles created
/// within that inner scope.
///
/// # Example
///
/// ```ignore
/// let saved = handle_stack.top();
/// let h1 = handle_stack.push(some_obj);  // h1 valid
/// let h2 = handle_stack.push(other_obj); // h2 valid
/// handle_stack.truncate(saved);          // h1, h2 invalid
/// ```
///
/// In practice, the `GcHeap` wraps this pattern in ergonomic scope methods.
pub struct HandleScopeLevel(u32);

impl HandleScopeLevel {
    /// Saves the current handle stack top.
    pub fn enter(stack: &HandleStack) -> Self {
        Self(stack.top())
    }

    /// Restores the handle stack to the saved level (drops all handles created
    /// after this scope was entered).
    pub fn exit(self, stack: &mut HandleStack) {
        stack.truncate(self.0);
    }

    /// The saved stack level.
    pub fn level(&self) -> u32 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::GcHeader;

    #[test]
    fn handle_stack_push_and_read() {
        let mut stack = HandleStack::new();
        let header = GcHeader::new(0, 16);
        let handle = stack.push(&header);

        assert_eq!(stack.get(handle), Some(&header as *const GcHeader));
        assert_eq!(stack.len(), 1);
    }

    #[test]
    fn handle_scope_truncates_on_exit() {
        let mut stack = HandleStack::new();
        let h1_header = GcHeader::new(0, 16);
        let _h1 = stack.push(&h1_header);

        // Enter inner scope.
        let scope = HandleScopeLevel::enter(&stack);

        let h2_header = GcHeader::new(1, 16);
        let h2 = stack.push(&h2_header);
        assert_eq!(stack.len(), 2);
        assert!(stack.get(h2).is_some());

        // Exit inner scope — h2 becomes invalid.
        scope.exit(&mut stack);
        assert_eq!(stack.len(), 1);
        assert!(stack.get(h2).is_none()); // Out of bounds now
    }

    #[test]
    fn nested_handle_scopes() {
        let mut stack = HandleStack::new();

        let outer = HandleScopeLevel::enter(&stack);
        let h1 = GcHeader::new(0, 16);
        stack.push(&h1);

        let inner = HandleScopeLevel::enter(&stack);
        let h2 = GcHeader::new(1, 16);
        stack.push(&h2);
        assert_eq!(stack.len(), 2);

        inner.exit(&mut stack);
        assert_eq!(stack.len(), 1); // h2 gone

        outer.exit(&mut stack);
        assert_eq!(stack.len(), 0); // h1 gone
    }

    #[test]
    fn handle_stack_root_slots() {
        let mut stack = HandleStack::new();
        let h1 = GcHeader::new(0, 16);
        let h2 = GcHeader::new(1, 16);
        stack.push(&h1);
        stack.push(&h2);

        let slots = stack.root_slots();
        assert_eq!(slots.len(), 2);

        // Verify the slots point to valid pointers.
        for slot in &slots {
            let ptr = unsafe { **slot };
            assert!(!ptr.is_null());
        }
    }

    #[test]
    fn global_handle_create_and_read() {
        let mut table = GlobalHandleTable::new();
        let header = GcHeader::new(5, 32);
        let handle = table.create(&header);

        assert_eq!(table.get(handle), Some(&header as *const GcHeader));
        assert_eq!(table.active_count(), 1);
    }

    #[test]
    fn global_handle_release_and_reuse() {
        let mut table = GlobalHandleTable::new();
        let h1 = GcHeader::new(0, 16);
        let h2 = GcHeader::new(1, 16);

        let gh1 = table.create(&h1);
        let _gh2 = table.create(&h2);
        assert_eq!(table.active_count(), 2);

        // Release gh1.
        table.release(gh1);
        assert_eq!(table.active_count(), 1);
        assert!(table.get(gh1).is_none());

        // New handle should reuse the freed slot.
        let h3 = GcHeader::new(2, 16);
        let gh3 = table.create(&h3);
        assert_eq!(gh3.index(), gh1.index()); // Reused slot
        assert_eq!(table.active_count(), 2);
    }

    #[test]
    fn global_handle_root_slots() {
        let mut table = GlobalHandleTable::new();
        let h1 = GcHeader::new(0, 16);
        let h2 = GcHeader::new(1, 16);
        table.create(&h1);
        let gh2 = table.create(&h2);

        // Release one.
        table.release(gh2);

        let slots = table.root_slots();
        assert_eq!(slots.len(), 1); // Only h1 is active
    }
}
