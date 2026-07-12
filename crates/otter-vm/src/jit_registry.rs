//! Isolate-owned registry of installed JIT code objects.
//!
//! The registry maps unique code-object ids to installed
//! [`crate::jit::JitFunctionCode`] objects and publishes one stable
//! [`CodeRegistryView`] so native code can resolve `(code_object_id,
//! safepoint_id)` to a precise root map for *any* installed object — the
//! entered function or a nested compiled callee alike.
//!
//! # Contents
//! - [`JitCodeRegistry`] — boxed, address-stable registry cell.
//! - `resolve_jit_registry_safepoint` — the machine-visible resolver behind
//!   the published view.
//!
//! # Invariants
//! - The registry is heap-boxed once at interpreter construction; its view and
//!   map addresses never move, so the published view survives interpreter
//!   moves.
//! - Registration happens only between VM turns (at compile install), never
//!   while the resolver can run inside a native call; the single-threaded
//!   isolate contract keeps reads and writes disjoint in time.
//! - A registered object is retained by the registry `Arc`, keeping every
//!   safepoint-record address it hands out alive.
//!
//! # See also
//! - [`crate::native_abi::CodeRegistryView`] — the published lookup surface.
//! - `JIT_REFACTOR_PLAN.md` Phase 4 for the lifetime states this backs.

use crate::jit::JitFunctionCode;
use crate::native_abi::{CodeLifetimeState, CodeRegistryView, SafepointId, SafepointRecord};
use std::sync::Arc;

/// One registered code object with its lifecycle state.
struct RegisteredCode {
    code: Arc<dyn JitFunctionCode>,
    state: CodeLifetimeState,
}

/// Isolate-owned installed-code registry behind a stable published view.
pub struct JitCodeRegistry {
    /// Published C-layout lookup surface; `context` names this registry.
    view: CodeRegistryView,
    /// Installed code objects by unique code-object id.
    codes: rustc_hash::FxHashMap<u64, RegisteredCode>,
}

impl JitCodeRegistry {
    /// Allocate the registry cell and wire its published view to itself.
    #[must_use]
    pub(crate) fn new_boxed() -> Box<Self> {
        let mut registry = Box::new(Self {
            view: CodeRegistryView {
                context: 0,
                resolve_safepoint: resolve_jit_registry_safepoint as *const () as u64,
            },
            codes: rustc_hash::FxHashMap::default(),
        });
        registry.view.context = std::ptr::addr_of!(*registry) as u64;
        registry
    }

    /// Register one installed code object under its unique id.
    pub(crate) fn register(&mut self, code_object_id: u64, code: Arc<dyn JitFunctionCode>) {
        debug_assert_ne!(code_object_id, 0);
        debug_assert_eq!(code.metadata().id, code_object_id);
        let replaced = self.codes.insert(
            code_object_id,
            RegisteredCode {
                code,
                state: CodeLifetimeState::Installed,
            },
        );
        debug_assert!(replaced.is_none(), "code-object ids are never reused");
    }

    /// Unlink every installed body compiled from `function_id`: the code takes
    /// no new entries, while active frames keep returning through it via their
    /// `Arc` anchors.
    pub(crate) fn invalidate_function(&mut self, function_id: u32) {
        for registered in self.codes.values_mut() {
            if registered.state == CodeLifetimeState::Installed
                && registered.code.metadata().code_block_id == function_id
            {
                registered.state = CodeLifetimeState::Invalid;
            }
        }
    }

    /// Retire invalid code whose last anchor is the registry itself: no map
    /// entry, cache, direct-call anchor, or active frame still references it,
    /// so the executable mapping is safe to reclaim. Returns how many objects
    /// retired.
    pub(crate) fn retire_unreferenced(&mut self) -> usize {
        let before = self.codes.len();
        self.codes.retain(|_, registered| {
            registered.state != CodeLifetimeState::Invalid
                || Arc::strong_count(&registered.code) > 1
        });
        before - self.codes.len()
    }

    /// Address of the published view for [`crate::native_abi::VmThread`].
    #[must_use]
    pub(crate) fn view_addr(&self) -> u64 {
        std::ptr::addr_of!(self.view) as u64
    }

    fn resolve(&self, code_object_id: u64, safepoint_id: SafepointId) -> *const SafepointRecord {
        // Invalid code still resolves: an active frame may be executing it and
        // its allocating stubs must keep rooting precisely until it retires.
        self.codes
            .get(&code_object_id)
            .and_then(|registered| registered.code.safepoint_record(safepoint_id))
            .map_or(std::ptr::null(), std::ptr::from_ref)
    }
}

impl std::fmt::Debug for JitCodeRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JitCodeRegistry")
            .field("codes", &self.codes.len())
            .finish()
    }
}

/// Machine-visible resolver behind the registry's published view.
///
/// # Safety
/// `context` must be the address of a live [`JitCodeRegistry`] cell published
/// by the owning isolate; the isolate keeps it boxed and unmutated for the
/// duration of any native call that can invoke this resolver.
unsafe extern "C" fn resolve_jit_registry_safepoint(
    context: u64,
    code_object_id: u64,
    safepoint_id: SafepointId,
) -> *const SafepointRecord {
    if context == 0 {
        return std::ptr::null();
    }
    // SAFETY: the publishing isolate keeps the boxed registry alive and
    // unmutated across the native call (see module invariants).
    let registry = unsafe { &*(context as *const JitCodeRegistry) };
    registry.resolve(code_object_id, safepoint_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_abi::{CodeObjectMetadata, NO_FRAME_STATE};

    #[derive(Debug)]
    struct FakeCode {
        id: u64,
        records: Vec<SafepointRecord>,
    }

    impl JitFunctionCode for FakeCode {
        fn metadata(&self) -> CodeObjectMetadata {
            let mut metadata = CodeObjectMetadata {
                id: self.id,
                code_block_id: 0,
                entry_offset: 0,
                code_size: 4,
                safepoint_count: self.records.len() as u32,
                frame_map_count: 0,
                spill_map_count: 0,
                dependency_count: 0,
                reserved: 0,
                layout: crate::native_abi::LayoutVersionRecord::CURRENT,
                build: crate::native_abi::BuildVersionRecord {
                    vm_build: crate::native_abi::VM_BUILD_VERSION,
                    target_abi: 1,
                },
            };
            metadata.safepoint_count = self.records.len() as u32;
            metadata
        }

        fn code_len(&self) -> usize {
            4
        }

        fn safepoint_record(&self, safepoint_id: SafepointId) -> Option<&SafepointRecord> {
            self.records
                .binary_search_by_key(&safepoint_id, |record| record.id)
                .ok()
                .map(|index| &self.records[index])
        }

        fn run_entry(&self, _activation: crate::VmRuntimeActivation) -> crate::jit::JitExecOutcome {
            unreachable!("fake code is never entered")
        }
    }

    #[test]
    fn resolves_safepoints_across_distinct_code_objects() {
        let mut registry = JitCodeRegistry::new_boxed();
        registry.register(
            7,
            Arc::new(FakeCode {
                id: 7,
                records: vec![SafepointRecord::frame_slot_window(3, NO_FRAME_STATE, 2)],
            }),
        );
        registry.register(
            9,
            Arc::new(FakeCode {
                id: 9,
                records: vec![SafepointRecord::frame_slot_window(5, NO_FRAME_STATE, 4)],
            }),
        );

        let view = unsafe { *(registry.view_addr() as *const CodeRegistryView) };
        let hit = unsafe { view.resolve(9, 5) }.expect("nested callee record resolves");
        assert_eq!(unsafe { (*hit).id }, 5);
        assert_eq!(unsafe { &*hit }.tagged_locations.len(), 4);
        let other = unsafe { view.resolve(7, 3) }.expect("entry record resolves");
        assert_eq!(unsafe { (*other).id }, 3);
        assert!(unsafe { view.resolve(7, 5) }.is_none(), "id is per-object");
        assert!(unsafe { view.resolve(8, 3) }.is_none(), "unknown object");
    }

    #[test]
    fn invalid_code_resolves_until_last_anchor_drops() {
        let mut registry = JitCodeRegistry::new_boxed();
        let code: Arc<dyn JitFunctionCode> = Arc::new(FakeCode {
            id: 11,
            records: vec![SafepointRecord::frame_slot_window(1, NO_FRAME_STATE, 2)],
        });
        let anchor = code.clone();
        registry.register(11, code);

        registry.invalidate_function(0);
        // An active anchor (a frame or direct-call pin) keeps invalid code
        // registered and resolvable.
        assert_eq!(registry.retire_unreferenced(), 0);
        let view = unsafe { *(registry.view_addr() as *const CodeRegistryView) };
        assert!(unsafe { view.resolve(11, 1) }.is_some());

        drop(anchor);
        assert_eq!(registry.retire_unreferenced(), 1);
        assert!(unsafe { view.resolve(11, 1) }.is_none());
    }
}
