//! Hidden Classes (Shapes) for property access optimization.
//!
//! A Shape represents the structure of an object: what properties it has
//! and at what offsets they are stored. Shapes are shared between objects
//! with the same structure using a transition tree.

use crate::object::PropertyKey;
use std::cell::RefCell;
use rustc_hash::FxHashMap;
use std::sync::{Arc, Weak};

/// A Shape defines the layout of properties in an object.
pub struct Shape {
    /// The parent shape from which this shape was transitioned.
    /// None for the root (empty) shape.
    pub parent: Option<Arc<Shape>>,

    /// The property key that was added to the parent to create this shape.
    pub key: Option<PropertyKey>,

    /// The offset of the property in the object's property vector.
    pub offset: Option<usize>,

    /// Transitions from this shape to child shapes when a new property is added.
    /// Use Weak to break cycles: Child -> Parent (Arc), Parent -> Child (Weak)
    /// RefCell is used since shape transitions are not on the IC fast path.
    transitions: RefCell<FxHashMap<PropertyKey, Weak<Shape>>>,

    /// Cache of all property offsets in this shape (inherited + own).
    /// This is built lazily or during creation for fast lookups.
    property_map: FxHashMap<PropertyKey, usize>,

    /// Keys in insertion order for JSON.stringify and Object.keys()
    keys_ordered: Vec<PropertyKey>,
}

// SAFETY: Shape is only accessed from a single VM thread.
// RefCell is !Sync, but our VM is thread-confined.
unsafe impl Send for Shape {}
unsafe impl Sync for Shape {}

impl Shape {
    /// Trace elements in this shape for GC
    pub fn trace(&self, tracer: &mut dyn crate::gc::Tracer) {
        // Trace the property key that created this shape
        if let Some(key) = &self.key {
            key.trace(tracer);
        }

        // Trace the parent shape to keep the transition path alive
        if let Some(parent) = &self.parent {
            tracer.mark(parent.as_ref());
        }

        // Note: transitions are not traced directly for GC as they are weak
    }

    /// Create a new root (empty) shape.
    pub fn root() -> Arc<Self> {
        Arc::new(Self {
            parent: None,
            key: None,
            offset: None,
            transitions: RefCell::new(FxHashMap::default()),
            property_map: FxHashMap::default(),
            keys_ordered: Vec::new(),
        })
    }

    /// Find a transition for a given key, or create a new one.
    pub fn transition(self: &Arc<Self>, key: PropertyKey) -> Arc<Self> {
        // Check if transition already exists
        {
            let transitions = self.transitions.borrow();
            if let Some(weak_shape) = transitions.get(&key) {
                if let Some(shape) = weak_shape.upgrade() {
                    return shape;
                }
            }
        }

        // Create new transition
        let mut transitions = self.transitions.borrow_mut();

        // Double-check after acquiring mutable borrow
        if let Some(weak_shape) = transitions.get(&key) {
            if let Some(shape) = weak_shape.upgrade() {
                return shape;
            }
        }

        let next_offset = self.offset.map(|o| o + 1).unwrap_or(0);

        let mut next_property_map = self.property_map.clone();
        next_property_map.insert(key.clone(), next_offset);

        let mut next_keys_ordered = self.keys_ordered.clone();
        next_keys_ordered.push(key.clone());

        let new_shape = Arc::new(Self {
            parent: Some(Arc::clone(self)),
            key: Some(key.clone()),
            offset: Some(next_offset),
            transitions: RefCell::new(FxHashMap::default()),
            property_map: next_property_map,
            keys_ordered: next_keys_ordered,
        });

        transitions.insert(key, Arc::downgrade(&new_shape));
        new_shape
    }

    /// Get the offset of a property key in this shape.
    pub fn get_offset(&self, key: &PropertyKey) -> Option<usize> {
        self.property_map.get(key).copied()
    }

    /// Get all own property keys in this shape in insertion order.
    pub fn own_keys(&self) -> Vec<PropertyKey> {
        self.keys_ordered.clone()
    }

    /// Get the number of properties defined in this shape.
    pub fn property_count(&self) -> usize {
        self.property_map.len()
    }
}

impl std::fmt::Debug for Shape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shape")
            .field("key", &self.key)
            .field("offset", &self.offset)
            .field("property_count", &self.property_count())
            .finish()
    }
}
