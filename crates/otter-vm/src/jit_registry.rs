//! Isolate-owned registry of installed JIT code objects.
//!
//! The registry maps unique code-object ids to installed
//! [`crate::jit::JitFunctionCode`] objects, snapshots their isolate-state
//! dependencies, and publishes one stable [`CodeRegistryView`] so native code
//! can resolve `(code_object_id, safepoint_id)` to a precise root map for *any*
//! installed object — the entered function or a nested compiled callee alike.
//!
//! # Contents
//! - [`JitCodeRegistry`] — boxed, address-stable registry cell.
//! - Dependency registration, exact-epoch entry consistency, and monotonic
//!   dependent invalidation.
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
//! - Dependency epochs are isolate-local and monotonic. Install and entry
//!   selection require `expected == current`; invalidation marks Installed
//!   code only when `expected < current` for the same `(kind, identity)`.
//! - Invalid code remains available to safepoint resolution until its last
//!   external anchor drops and [`JitCodeRegistry::retire_unreferenced`] removes
//!   it. Safepoint resolution therefore does not apply the entry check.
//!
//! # See also
//! - [`crate::native_abi::CodeRegistryView`] — the published lookup surface.
//! - `JIT_REFACTOR_PLAN.md` Phase 4 for the lifetime states this backs.

use crate::jit::JitFunctionCode;
use crate::native_abi::{
    CodeDependency, CodeDependencyKind, CodeLifetimeState, CodeRegistryView, SafepointId,
    SafepointRecord,
};
use std::sync::Arc;

/// One registered code object with its lifecycle state.
struct RegisteredCode {
    code: Arc<dyn JitFunctionCode>,
    dependencies: Box<[CodeDependency]>,
    state: CodeLifetimeState,
}

/// Isolate-owned installed-code registry behind a stable published view.
pub struct JitCodeRegistry {
    /// Published C-layout lookup surface; `context` names this registry.
    view: CodeRegistryView,
    /// Installed code objects by unique code-object id.
    codes: rustc_hash::FxHashMap<u64, RegisteredCode>,
    /// Latest isolate-local epoch by dependency family and stable identity.
    epochs: rustc_hash::FxHashMap<(CodeDependencyKind, u32), u64>,
    /// Monotonic counter bumped whenever any installed code can become
    /// invalid (function invalidation or dependency-epoch publication).
    /// Cached per-call-site entry plans snapshot it; an equal snapshot proves
    /// the cached plan's compatibility check is still current without
    /// re-walking the registry.
    invalidation_epoch: u64,
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
            epochs: rustc_hash::FxHashMap::default(),
            invalidation_epoch: 0,
        });
        registry.view.context = std::ptr::addr_of!(*registry) as u64;
        registry
    }

    /// Register one current code object under its unique id.
    ///
    /// Returns `false` without installing when metadata is incompatible, the
    /// declared count differs from the dependency slice, or any dependency is
    /// not exactly current. The code object owns the declaration surface; the
    /// registry snapshots it so later invalidation never depends on a virtual
    /// call into mutable compiler state.
    pub(crate) fn register(&mut self, code_object_id: u64, code: Arc<dyn JitFunctionCode>) -> bool {
        debug_assert_ne!(code_object_id, 0);
        debug_assert_eq!(code.metadata().id, code_object_id);
        let metadata = code.metadata();
        let dependencies: Box<[CodeDependency]> = code.dependencies().into();
        if !metadata.is_compatible_with_current_vm()
            || metadata.dependency_count as usize != dependencies.len()
            || !self.dependencies_are_current(&dependencies)
        {
            return false;
        }
        let replaced = self.codes.insert(
            code_object_id,
            RegisteredCode {
                code,
                dependencies,
                state: CodeLifetimeState::Installed,
            },
        );
        debug_assert!(replaced.is_none(), "code-object ids are never reused");
        true
    }

    /// Whether `code` passes layout/build compatibility and exact dependency
    /// epoch consistency for a new entry selection.
    ///
    /// The zero-dependency branch is deliberately identical to the pre-epoch
    /// metadata gate and does not consult registry lifetime state. Existing
    /// bail/reoptimization invalidation owns its cache unlinking separately.
    pub(crate) fn is_compatible_for_entry(&self, code: &dyn JitFunctionCode) -> bool {
        let metadata = code.metadata();
        metadata.is_compatible_with_current_vm() && self.dependencies_are_current_for_entry(code)
    }

    /// Whether only the declared dependency slice is exactly current.
    ///
    /// Kept separate from layout/build compatibility so OSR can add the epoch
    /// gate without changing its existing template-owned layout mismatch path.
    pub(crate) fn dependencies_are_current_for_entry(&self, code: &dyn JitFunctionCode) -> bool {
        let metadata = code.metadata();
        if metadata.dependency_count == 0 {
            return true;
        }
        self.codes.get(&metadata.id).is_some_and(|registered| {
            metadata.dependency_count as usize == registered.dependencies.len()
                && self.dependencies_are_current(&registered.dependencies)
        })
    }

    /// Unlink every installed body compiled from `function_id`: the code takes
    /// no new entries, while active frames keep returning through it via their
    /// `Arc` anchors.
    pub(crate) fn invalidate_function(&mut self, function_id: u32) {
        self.invalidation_epoch += 1;
        for registered in self.codes.values_mut() {
            if registered.state == CodeLifetimeState::Installed
                && registered.code.metadata().code_block_id == function_id
            {
                registered.state = CodeLifetimeState::Invalid;
            }
        }
    }

    /// Publish `current_epoch` and invalidate Installed code whose matching
    /// dependency is stale.
    ///
    /// Epochs never move backwards. A redundant publication is a no-op; a
    /// lower publication is rejected by a debug assertion and ignored in
    /// release builds. Dependencies at exactly the current epoch remain
    /// Installed, while future dependencies are left Installed but fail the
    /// exact-equality install/entry consistency check.
    pub(crate) fn invalidate_dependents(
        &mut self,
        kind: CodeDependencyKind,
        identity: u32,
        current_epoch: u64,
    ) {
        let published = self.epochs.entry((kind, identity)).or_insert(0);
        debug_assert!(
            current_epoch >= *published,
            "dependency epochs must not move backwards"
        );
        if current_epoch <= *published {
            return;
        }
        *published = current_epoch;
        self.invalidation_epoch += 1;
        for registered in self.codes.values_mut() {
            if registered.state == CodeLifetimeState::Installed
                && registered.dependencies.iter().any(|dependency| {
                    dependency.kind == kind
                        && dependency.identity == identity
                        && dependency.expected < current_epoch
                })
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

    /// Current invalidation-epoch snapshot for cached per-site entry plans.
    #[must_use]
    pub(crate) fn invalidation_epoch(&self) -> u64 {
        self.invalidation_epoch
    }

    /// Address of the published view for [`crate::native_abi::VmThread`].
    #[must_use]
    pub(crate) fn view_addr(&self) -> u64 {
        std::ptr::addr_of!(self.view) as u64
    }

    fn dependencies_are_current(&self, dependencies: &[CodeDependency]) -> bool {
        dependencies.iter().all(|dependency| {
            dependency.flags == 0
                && dependency.expected
                    == self
                        .epochs
                        .get(&(dependency.kind, dependency.identity))
                        .copied()
                        .unwrap_or(0)
        })
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
    use crate::native_abi::{
        ARRAY_INDEX_ACCESSOR_PROTECTOR_IDENTITY, CodeObjectMetadata, NO_FRAME_STATE,
        ORDINARY_OBJECT_PROTOTYPE_SHAPE_IDENTITY,
    };

    #[derive(Debug)]
    struct FakeCode {
        id: u64,
        records: Vec<SafepointRecord>,
        dependencies: Box<[CodeDependency]>,
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
                dependency_count: self.dependencies.len() as u32,
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

        fn dependencies(&self) -> &[CodeDependency] {
            &self.dependencies
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
        assert!(registry.register(
            7,
            Arc::new(FakeCode {
                id: 7,
                records: vec![SafepointRecord::frame_slot_window(3, NO_FRAME_STATE, 2)],
                dependencies: Vec::new().into_boxed_slice(),
            }),
        ));
        assert!(registry.register(
            9,
            Arc::new(FakeCode {
                id: 9,
                records: vec![SafepointRecord::frame_slot_window(5, NO_FRAME_STATE, 4)],
                dependencies: Vec::new().into_boxed_slice(),
            }),
        ));

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
            dependencies: Vec::new().into_boxed_slice(),
        });
        let anchor = code.clone();
        assert!(registry.register(11, code));

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

    fn fake_code(id: u64, dependencies: Vec<CodeDependency>) -> Arc<dyn JitFunctionCode> {
        Arc::new(FakeCode {
            id,
            records: Vec::new(),
            dependencies: dependencies.into_boxed_slice(),
        })
    }

    #[test]
    fn protector_bump_invalidates_only_stale_matching_dependencies() {
        let mut interp = crate::Interpreter::new();
        let protector = CodeDependency::epoch(
            CodeDependencyKind::Protector,
            ARRAY_INDEX_ACCESSOR_PROTECTOR_IDENTITY,
            0,
        );
        assert!(
            interp
                .jit_code_registry
                .register(21, fake_code(21, vec![protector]))
        );
        assert!(
            interp
                .jit_code_registry
                .register(22, fake_code(22, Vec::new()))
        );
        assert!(interp.jit_code_registry.register(
            23,
            fake_code(
                23,
                vec![CodeDependency::epoch(
                    CodeDependencyKind::Protector,
                    ARRAY_INDEX_ACCESSOR_PROTECTOR_IDENTITY + 1,
                    0,
                )],
            ),
        ));

        interp.activate_array_index_accessor_protector();

        assert_eq!(
            interp.jit_code_registry.codes[&21].state,
            CodeLifetimeState::Invalid
        );
        assert_eq!(
            interp.jit_code_registry.codes[&22].state,
            CodeLifetimeState::Installed
        );
        assert_eq!(
            interp.jit_code_registry.codes[&23].state,
            CodeLifetimeState::Installed
        );
        assert!(
            !interp
                .jit_code_registry
                .is_compatible_for_entry(interp.jit_code_registry.codes[&21].code.as_ref())
        );

        let current = CodeDependency::epoch(
            CodeDependencyKind::Protector,
            ARRAY_INDEX_ACCESSOR_PROTECTOR_IDENTITY,
            interp.array_index_accessor_protector_epoch(),
        );
        assert!(
            interp
                .jit_code_registry
                .register(24, fake_code(24, vec![current]))
        );
        interp.jit_code_registry.invalidate_dependents(
            CodeDependencyKind::Protector,
            ARRAY_INDEX_ACCESSOR_PROTECTOR_IDENTITY,
            interp.array_index_accessor_protector_epoch(),
        );
        assert_eq!(
            interp.jit_code_registry.codes[&24].state,
            CodeLifetimeState::Installed
        );
        assert!(
            interp
                .jit_code_registry
                .is_compatible_for_entry(interp.jit_code_registry.codes[&24].code.as_ref())
        );
    }

    #[test]
    fn array_index_accessor_protector_epoch_advances_once() {
        let mut interp = crate::Interpreter::new();
        assert!(!interp.array_index_accessor_protector);
        assert_eq!(interp.array_index_accessor_protector_epoch(), 0);

        interp.activate_array_index_accessor_protector();
        assert!(interp.array_index_accessor_protector);
        assert_eq!(interp.array_index_accessor_protector_epoch(), 1);
        assert_eq!(interp.array_index_accessor_protector_epoch(), 1);

        interp.activate_array_index_accessor_protector();
        assert!(interp.array_index_accessor_protector);
        assert_eq!(interp.array_index_accessor_protector_epoch(), 1);
    }

    #[test]
    fn register_requires_exact_current_dependency_epoch() {
        let mut interp = crate::Interpreter::new();
        interp.activate_array_index_accessor_protector();

        for (id, expected) in [(31, 0), (32, 2)] {
            let dependency = CodeDependency::epoch(
                CodeDependencyKind::Protector,
                ARRAY_INDEX_ACCESSOR_PROTECTOR_IDENTITY,
                expected,
            );
            assert!(
                !interp
                    .jit_code_registry
                    .register(id, fake_code(id, vec![dependency]))
            );
            assert!(!interp.jit_code_registry.codes.contains_key(&id));
        }

        let current = CodeDependency::epoch(
            CodeDependencyKind::Protector,
            ARRAY_INDEX_ACCESSOR_PROTECTOR_IDENTITY,
            1,
        );
        assert!(
            interp
                .jit_code_registry
                .register(33, fake_code(33, vec![current]))
        );
    }

    #[test]
    fn ordinary_object_prototype_change_bumps_shape_epoch_once_per_change() {
        let mut interp = crate::Interpreter::new();
        let dependency = CodeDependency::epoch(
            CodeDependencyKind::ShapeEpoch,
            ORDINARY_OBJECT_PROTOTYPE_SHAPE_IDENTITY,
            0,
        );
        assert!(
            interp
                .jit_code_registry
                .register(41, fake_code(41, vec![dependency]))
        );
        let target = crate::object::alloc_object_old_for_fixture(interp.gc_heap_mut())
            .expect("target object");
        let prototype = crate::object::alloc_object_old_for_fixture(interp.gc_heap_mut())
            .expect("prototype object");
        let context = crate::ExecutionContext::from_module(crate::BytecodeModule {
            module: "shape-epoch-test.js".to_string(),
            template_sites: Vec::new(),
            source_kind: otter_bytecode::SourceKind::TypeScript,
            functions: Vec::new(),
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        });
        let target_value = crate::Value::object(target);
        let prototype_value = crate::Value::object(prototype);

        assert!(
            interp
                .set_prototype_value_proxy_aware(&context, &target_value, &prototype_value)
                .expect("ordinary prototype mutation")
        );
        assert_eq!(interp.shape_epoch(), 1);
        assert_eq!(
            interp.jit_code_registry.codes[&41].state,
            CodeLifetimeState::Invalid
        );

        assert!(
            interp
                .set_prototype_value_proxy_aware(&context, &target_value, &prototype_value)
                .expect("same prototype is accepted")
        );
        assert_eq!(interp.shape_epoch(), 1);
    }
}
