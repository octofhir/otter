//! Hidden Classes (Shapes) for property access optimization.
//!
//! A Shape represents the structure of an object: what properties it has
//! and at what offsets they are stored. Shapes are shared between objects
//! with the same structure using a transition tree.
//!
//! ## Walk-Up Architecture
//!
//! Instead of cloning a full property map on every transition (O(N) per property,
//! O(N^2) total for N properties), each Shape stores only its own (key, offset)
//! pair plus a parent pointer. Property lookups walk up the parent chain.
//! A lazy cached map is built for shapes deeper than SNAPSHOT_DEPTH to bound
//! walk-up cost.

use crate::object::PropertyKey;
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::sync::{Arc, Weak};

/// Depth threshold at which we cache the full property map.
/// Lookups on shapes shallower than this walk the parent chain directly.
/// At this depth, we snapshot the full map for O(1) lookups.
const SNAPSHOT_DEPTH: u16 = 8;

/// A Shape defines the layout of properties in an object.
pub struct Shape {
    /// The parent shape from which this shape was transitioned.
    /// None for the root (empty) shape.
    pub parent: Option<Arc<Shape>>,

    /// The property key that was added to the parent to create this shape.
    pub key: Option<PropertyKey>,

    /// The offset of the property in the object's property vector.
    pub offset: Option<usize>,

    /// Depth in the shape chain (root = 0).
    depth: u16,

    /// Transitions from this shape to child shapes when a new property is added.
    /// Use Weak to break cycles: Child -> Parent (Arc), Parent -> Child (Weak)
    /// RefCell is used since shape transitions are not on the IC fast path.
    transitions: RefCell<FxHashMap<PropertyKey, Weak<Shape>>>,

    /// Lazily-built cache of all property offsets. Populated on first lookup
    /// when depth >= SNAPSHOT_DEPTH, or on demand via `ensure_cached_map()`.
    cached_map: RefCell<Option<FxHashMap<PropertyKey, usize>>>,
    /// Unique identifier for this shape (non-reused across process lifetime).
    /// Used for stable Inline Cache hits even if shapes are re-allocated.
    pub id: u64,
}

static NEXT_SHAPE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_shape_id() -> u64 {
    NEXT_SHAPE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

// SAFETY: Shape is only accessed from a single VM thread.
// Thread confinement is enforced by the Isolate abstraction: each Isolate
// is `Send` but `!Sync`, ensuring only one thread accesses shapes at any time.
// The `Sync` impl is required for `GcRef<Shape>` bounds (`T: Send + Sync`)
// and for Arc<Shape> to be `Send`. RefCell transitions are thread-confined.
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

    /// Trace all GC-managed property keys in this shape for the garbage collector.
    ///
    /// Walks the parent chain from this shape to root, tracing each key.
    /// Also traces keys in the transitions map.
    pub fn trace_keys(&self, tracer: &mut dyn FnMut(*const crate::gc::GcHeader)) {
        // Walk the chain from this shape to root, tracing each key
        let mut current = Some(self);
        while let Some(shape) = current {
            if let Some(key) = &shape.key {
                match key {
                    PropertyKey::String(s) => tracer(s.header() as *const _),
                    PropertyKey::Symbol(sym) => tracer(sym.header() as *const _),
                    PropertyKey::Index(_) => {}
                }
            }
            // Also trace keys in this shape's transitions map
            for (key, _) in shape.transitions.borrow().iter() {
                match key {
                    PropertyKey::String(s) => tracer(s.header() as *const _),
                    PropertyKey::Symbol(sym) => tracer(sym.header() as *const _),
                    PropertyKey::Index(_) => {}
                }
            }
            current = shape.parent.as_deref();
        }
    }

    /// Create a new root (empty) shape.
    pub fn root() -> Arc<Self> {
        Arc::new(Self {
            parent: None,
            key: None,
            offset: None,
            depth: 0,
            transitions: RefCell::new(FxHashMap::default()),
            cached_map: RefCell::new(None),
            id: next_shape_id(),
        })
    }

    /// Find a transition for a given key, or create a new one.
    ///
    /// O(1) — no cloning of property maps or key vectors.
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

        // Prune dead Weak entries so the map is bounded by currently live shapes.
        transitions.retain(|_, w| w.strong_count() > 0);

        let next_offset = self.offset.map(|o| o + 1).unwrap_or(0);
        let next_depth = self.depth + 1;

        let new_shape = Arc::new(Self {
            parent: Some(Arc::clone(self)),
            key: Some(key.clone()),
            offset: Some(next_offset),
            depth: next_depth,
            transitions: RefCell::new(FxHashMap::default()),
            cached_map: RefCell::new(None),
            id: next_shape_id(),
        });

        transitions.insert(key, Arc::downgrade(&new_shape));
        new_shape
    }

    /// Get the offset of a property key in this shape.
    ///
    /// For shallow shapes (depth < SNAPSHOT_DEPTH), walks the parent chain.
    /// For deeper shapes, uses a lazily-built cached map for O(1) lookup.
    pub fn get_offset(&self, key: &PropertyKey) -> Option<usize> {
        // For deep shapes, use or build the cached map
        if self.depth >= SNAPSHOT_DEPTH {
            let map = self.cached_map.borrow();
            if let Some(ref m) = *map {
                return m.get(key).copied();
            }
            drop(map);
            // Build the cache
            self.ensure_cached_map();
            return self.cached_map.borrow().as_ref().unwrap().get(key).copied();
        }

        // For shallow shapes, walk the parent chain directly
        self.walk_get_offset(key)
    }

    /// Walk up the parent chain looking for a key. O(depth).
    #[inline]
    fn walk_get_offset(&self, key: &PropertyKey) -> Option<usize> {
        let mut current = Some(self);
        while let Some(shape) = current {
            if let Some(ref k) = shape.key {
                if k == key {
                    return shape.offset;
                }
            }
            current = shape.parent.as_deref();
        }
        None
    }

    /// Build and cache the full property map for this shape.
    fn ensure_cached_map(&self) {
        let mut map_ref = self.cached_map.borrow_mut();
        if map_ref.is_some() {
            return;
        }

        let mut map = FxHashMap::default();
        let mut current = Some(self);
        while let Some(shape) = current {
            if let (Some(k), Some(off)) = (&shape.key, shape.offset) {
                // Don't overwrite — first seen (closest to leaf) wins.
                // But in our linear chain, each key appears exactly once.
                map.entry(k.clone()).or_insert(off);
            }
            current = shape.parent.as_deref();
        }

        *map_ref = Some(map);
    }

    /// Get all own property keys in this shape in insertion order.
    ///
    /// Walks the parent chain from root to leaf (insertion order).
    /// ES2023 §9.1.11 requires: integer indices ascending, then string keys
    /// in insertion order, then symbols. The caller (JsObject::own_keys)
    /// handles the sorting — this method returns keys in chain order.
    pub fn own_keys(&self) -> Vec<PropertyKey> {
        if self.depth == 0 {
            return Vec::new();
        }

        // Collect keys by walking to root, then reverse for insertion order
        let mut keys = Vec::with_capacity(self.depth as usize);
        let mut current = Some(self);
        while let Some(shape) = current {
            if let Some(ref k) = shape.key {
                keys.push(k.clone());
            }
            current = shape.parent.as_deref();
        }
        keys.reverse();
        keys
    }

    /// Get the number of properties defined in this shape.
    pub fn property_count(&self) -> usize {
        self.depth as usize
    }
}

impl std::fmt::Debug for Shape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shape")
            .field("key", &self.key)
            .field("offset", &self.offset)
            .field("depth", &self.depth)
            .field("property_count", &self.property_count())
            .finish()
    }
}
