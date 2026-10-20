//! Per-isolate host state that native functions read instead of capturing.
//!
//! A host builtin that needs runtime state — the working directory, the
//! clock origin, a capability-bound network transport — finds it here by
//! type through [`crate::NativeCtx::host_service`]. The callable itself then
//! stays a static entry point, so the heap names no closure: a startup image
//! carries it as is, and the restoring host installs its own state.
//!
//! # Contents
//! - [`HostServices`] — one value per type, owned by the interpreter, and
//!   the host-data types whose bootstrap instances the host restores.
//!
//! # Invariants
//! - A service holds no `Value` or GC handle: nothing traces it. Services
//!   are `Send + Sync`, which no `Value` is, so the type system keeps them
//!   out.
//! - Services belong to one interpreter; nothing is shared through a static.
//! - Inserting a service of a type already present replaces it.
//! - A restore severs every host object's payload. A snapshot carries a host
//!   object only when its payload type is declared restorable here: the
//!   host's run services give such objects fresh payloads after a restore
//!   ([`crate::object::restore_host_data`]).
//!
//! # See also
//! - [`crate::snapshot`] — the startup image these keep closure-free.

use std::any::{Any, TypeId};

/// One value per type, looked up by type.
#[derive(Default)]
pub struct HostServices {
    entries: Vec<(TypeId, Box<dyn Any + Send + Sync>)>,
    /// Host-data types the host gives fresh payloads after a restore.
    restorable_host_data: Vec<TypeId>,
}

impl std::fmt::Debug for HostServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostServices")
            .field("count", &self.entries.len())
            .finish()
    }
}

impl HostServices {
    /// Install `service`, replacing one of the same type.
    pub fn insert<T: Any + Send + Sync>(&mut self, service: T) {
        let id = TypeId::of::<T>();
        match self.entries.iter_mut().find(|(entry, _)| *entry == id) {
            Some((_, slot)) => *slot = Box::new(service),
            None => self.entries.push((id, Box::new(service))),
        }
    }

    /// The service of type `T`, if installed.
    #[must_use]
    pub fn get<T: Any + Send + Sync>(&self) -> Option<&T> {
        let id = TypeId::of::<T>();
        self.entries
            .iter()
            .find(|(entry, _)| *entry == id)
            .and_then(|(_, service)| service.downcast_ref::<T>())
    }

    /// Declare that the host gives a bootstrap host object with a payload
    /// of type `T` a fresh one after a restore, so a snapshot may carry it.
    pub fn declare_restorable_host_data<T: Any>(&mut self) {
        let id = TypeId::of::<T>();
        if !self.restorable_host_data.contains(&id) {
            self.restorable_host_data.push(id);
        }
    }

    /// Whether host payloads of type `id` are restored by the host.
    #[must_use]
    pub fn restores_host_data(&self, id: TypeId) -> bool {
        self.restorable_host_data.contains(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::HostServices;

    #[test]
    fn a_service_is_found_by_type_and_replaced_by_its_type() {
        let mut services = HostServices::default();
        services.insert(7u32);
        services.insert(String::from("a"));
        services.insert(9u32);
        assert_eq!(services.get::<u32>(), Some(&9));
        assert_eq!(services.get::<String>().map(String::as_str), Some("a"));
        assert!(services.get::<u64>().is_none());
    }
}
