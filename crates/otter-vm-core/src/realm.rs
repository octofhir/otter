//! Realm registry and metadata.
//!
//! A Realm owns its own intrinsics and Function.prototype but shares the
//! MemoryManager with other realms.

use parking_lot::RwLock;
use std::sync::{Arc, atomic::{AtomicU32, Ordering}};

use crate::gc::GcRef;
use crate::intrinsics::Intrinsics;
use crate::object::JsObject;

/// Unique realm identifier.
pub type RealmId = u32;

/// Stored realm record.
#[derive(Clone)]
pub struct RealmRecord {
    pub id: RealmId,
    pub intrinsics: Intrinsics,
    pub function_prototype: GcRef<JsObject>,
    pub global: GcRef<JsObject>,
}

/// Registry of all realms created by a runtime.
pub struct RealmRegistry {
    realms: RwLock<Vec<RealmRecord>>,
    next_id: AtomicU32,
}

impl RealmRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            realms: RwLock::new(Vec::new()),
            next_id: AtomicU32::new(0),
        })
    }

    /// Allocate a new realm id.
    pub fn allocate_id(&self) -> RealmId {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Insert a realm record.
    pub fn insert(&self, record: RealmRecord) {
        self.realms.write().push(record);
    }

    /// Lookup a realm record by id.
    pub fn get(&self, id: RealmId) -> Option<RealmRecord> {
        self.realms.read().iter().find(|r| r.id == id).cloned()
    }

    /// Remove a realm record by id, dropping GcRef roots so GC can collect old objects.
    pub fn remove(&self, id: RealmId) -> Option<RealmRecord> {
        let mut realms = self.realms.write();
        if let Some(pos) = realms.iter().position(|r| r.id == id) {
            Some(realms.swap_remove(pos))
        } else {
            None
        }
    }
}
