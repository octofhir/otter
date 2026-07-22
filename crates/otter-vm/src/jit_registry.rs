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
//! - Stable per-function entry dispatch plus per-generation
//!   [`crate::native_abi::CodeEntryCell`] installation, unlinking, and
//!   tombstone retention.
//! - Dependency registration, exact-epoch entry consistency, and monotonic
//!   invalidation.
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
//!   external anchor drops and the interpreter reaches a native-activation
//!   retirement epoch before [`JitCodeRegistry::retire_unreferenced`] removes
//!   it. Safepoint resolution therefore does not apply the entry check.
//! - Generated callers retain only stable function-cell addresses. Publishing a
//!   new generation never invalidates or recompiles dependent callers.
//! - Invalidating a generation unlinks its entry cell before executable
//!   retirement. The cell address remains valid and is never reused.
//!
//! # See also
//! - [`crate::native_abi::CodeRegistryView`] — the published lookup surface.
//! - `OTTER_PLAN.md` for the active JIT direction and lifetime invariants.

use crate::jit::{
    JitCodeGenerationSnapshot, JitDirectCallPlan, JitDirectCallThisMode, JitFunctionCode,
};
use crate::native_abi::{
    CODE_ENTRY_HAS_SAFEPOINTS, CODE_ENTRY_OPTIMIZING_TIER, CodeDependency, CodeDependencyKind,
    CodeEntryCell, CodeLifetimeState, CodeRegistryView, FunctionEntryCell, NativeFrameKind,
    SafepointId, SafepointRecord,
};
use std::sync::Arc;

/// One registered code object with its lifecycle state.
struct RegisteredCode {
    code: Arc<dyn JitFunctionCode>,
    dependencies: Box<[CodeDependency]>,
    state: CodeLifetimeState,
}

/// New generated-call observations since the previous cold reconciliation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GeneratedCallFeedback {
    pub(crate) function_id: u32,
    pub(crate) code_object_id: u64,
    pub(crate) tier: NativeFrameKind,
    pub(crate) entries: u64,
    pub(crate) returns: u64,
    pub(crate) deopts: u64,
    pub(crate) throws: u64,
}

/// Current direct-call health for one exact generated code object.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GeneratedDeoptState {
    pub(crate) function_id: u32,
    pub(crate) tier: NativeFrameKind,
    pub(crate) entries: u64,
    pub(crate) deopts: u64,
    pub(crate) consecutive_deopts: u32,
    pub(crate) linked: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct GeneratedFeedbackSeen {
    entries: u64,
    returns: u64,
    deopts: u64,
    throws: u64,
}

/// Isolate-owned installed-code registry behind a stable published view.
pub struct JitCodeRegistry {
    /// Published C-layout lookup surface; `context` names this registry.
    view: CodeRegistryView,
    /// Installed code objects by unique code-object id.
    codes: rustc_hash::FxHashMap<u64, RegisteredCode>,
    /// Address-stable entry cells by code generation. Cells are tombstoned on
    /// invalidation and intentionally survive executable retirement so a baked
    /// pointer can never observe freed or repurposed metadata.
    entry_cells: rustc_hash::FxHashMap<u64, Box<CodeEntryCell>>,
    /// Permanent generated-call linkage cells by bytecode function. Callers
    /// bake these addresses once; tier publication switches only the contained
    /// generation-cell pointer.
    function_entry_cells: rustc_hash::FxHashMap<u32, Box<FunctionEntryCell>>,
    /// Last cumulative generated-call counters merged into VM tier policy.
    generated_feedback_seen: rustc_hash::FxHashMap<u64, GeneratedFeedbackSeen>,
    /// Latest isolate-local epoch by dependency family and stable identity.
    epochs: rustc_hash::FxHashMap<(CodeDependencyKind, u32), u64>,
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
            entry_cells: rustc_hash::FxHashMap::default(),
            function_entry_cells: rustc_hash::FxHashMap::default(),
            generated_feedback_seen: rustc_hash::FxHashMap::default(),
            epochs: rustc_hash::FxHashMap::default(),
        });
        registry.view.context = std::ptr::addr_of!(*registry) as u64;
        registry
    }

    /// Register one current code object under its unique id.
    ///
    /// Returns `false` without installing when the metadata identity or code
    /// size is invalid, the declared count differs from the dependency slice,
    /// or any dependency is not exactly current. The code object owns the
    /// declaration surface; the registry snapshots it so later invalidation
    /// never depends on a virtual call into mutable compiler state.
    #[cfg(test)]
    pub(crate) fn register(&mut self, code_object_id: u64, code: Arc<dyn JitFunctionCode>) -> bool {
        self.register_inner(code_object_id, code, None)
    }

    /// Install one compiled body using authoritative CodeBlock layout.
    ///
    /// Compile callers provide the exact verified function and code owner; the
    /// registry derives identity, frame counts, safepoint flags, and stable
    /// entry metadata internally. No caller assembles a native entry cell.
    pub(crate) fn install_compiled(
        &mut self,
        expected_code_object_id: u64,
        code: Arc<dyn JitFunctionCode>,
        function: &crate::executable::CodeBlock,
    ) -> bool {
        let metadata = code.metadata();
        if metadata.id != expected_code_object_id || metadata.code_block_id != function.id {
            return false;
        }
        self.register_generation(
            metadata.id,
            code,
            function.param_count,
            function.register_count,
        )
    }

    /// Low-level generation registration used by the high-level installer and
    /// focused registry fixtures.
    fn register_generation(
        &mut self,
        code_object_id: u64,
        code: Arc<dyn JitFunctionCode>,
        param_count: u16,
        register_count: u16,
    ) -> bool {
        let Some(entry_addr) = code.entry_addr() else {
            return false;
        };
        if entry_addr == 0 {
            return false;
        }
        let metadata = code.metadata();
        let mut flags = 0;
        if code.safepoint_count() != 0 {
            flags |= CODE_ENTRY_HAS_SAFEPOINTS;
        }
        if code.native_frame_kind() == NativeFrameKind::Optimizing {
            flags |= CODE_ENTRY_OPTIMIZING_TIER;
        }
        let entry_cell = Box::new(CodeEntryCell::new(
            entry_addr,
            code_object_id,
            metadata.code_block_id,
            param_count,
            register_count,
            flags,
            code.generated_stack_frame_bytes().unwrap_or(0),
        ));
        self.register_inner(code_object_id, code, Some(entry_cell))
    }

    fn register_inner(
        &mut self,
        code_object_id: u64,
        code: Arc<dyn JitFunctionCode>,
        entry_cell: Option<Box<CodeEntryCell>>,
    ) -> bool {
        debug_assert_ne!(code_object_id, 0);
        debug_assert_eq!(code.metadata().id, code_object_id);
        let metadata = code.metadata();
        let dependencies: Box<[CodeDependency]> = code.dependencies().into();
        if code_object_id == 0
            || metadata.id != code_object_id
            || metadata.code_size == 0
            || metadata.dependency_count as usize != dependencies.len()
            || !self.dependencies_are_current(&dependencies)
        {
            return false;
        }
        if self.codes.contains_key(&code_object_id)
            || self.entry_cells.contains_key(&code_object_id)
        {
            debug_assert!(false, "code-object ids are never reused");
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
        if let Some(entry_cell) = entry_cell {
            let function_id = entry_cell.function_id;
            let param_count = entry_cell.param_count;
            let register_count = entry_cell.register_count;
            let function_entry =
                self.function_entry_cells
                    .entry(function_id)
                    .or_insert_with(|| {
                        Box::new(FunctionEntryCell::new(
                            function_id,
                            param_count,
                            register_count,
                        ))
                    });
            if function_entry.param_count != param_count
                || function_entry.register_count != register_count
            {
                debug_assert!(
                    false,
                    "function entry layout cannot change across generations"
                );
                self.codes.remove(&code_object_id);
                return false;
            }
            let replaced = self.entry_cells.insert(code_object_id, entry_cell);
            debug_assert!(replaced.is_none(), "entry-cell ids are never reused");
            self.refresh_function_entry(function_id);
        }
        true
    }

    /// Whether this exact installed generation remains current for entry.
    ///
    /// Entry requires the exact registered generation to remain Installed even
    /// when it declares no external dependencies. Cache eviction is therefore
    /// an optimization, not the only correctness barrier after invalidation.
    pub(crate) fn is_current_for_entry(&self, code: &dyn JitFunctionCode) -> bool {
        let metadata = code.metadata();
        self.codes.get(&metadata.id).is_some_and(|registered| {
            registered.state == CodeLifetimeState::Installed
                && std::ptr::eq::<dyn JitFunctionCode>(registered.code.as_ref(), code)
                && metadata.dependency_count as usize == registered.dependencies.len()
                && self.dependencies_are_current(&registered.dependencies)
                && self.entry_cells.get(&metadata.id).is_none_or(|cell| {
                    cell.entry_addr.load(std::sync::atomic::Ordering::Acquire) != 0
                })
        })
    }

    /// Resolve one stable function entry into the complete tier-neutral
    /// direct-call plan consumed by generated frame construction.
    ///
    /// The plan snapshots current generation metadata for layout diagnostics,
    /// but generated code bakes only the permanent function-cell address. A
    /// later promotion changes the target generation without changing callers.
    #[must_use]
    pub(crate) fn direct_call_plan(
        &self,
        function: &crate::executable::CodeBlock,
    ) -> Option<JitDirectCallPlan> {
        let function_entry = self.function_entry_cells.get(&function.id)?;
        let generation_addr = function_entry.current_generation();
        if generation_addr == 0 {
            return None;
        }
        let generation = self
            .entry_cells
            .values()
            .find(|cell| std::ptr::from_ref(cell.as_ref()) as u64 == generation_addr)?;
        let registered = self.codes.get(&generation.code_object_id)?;
        if registered.state != CodeLifetimeState::Installed
            || !self.dependencies_are_current(&registered.dependencies)
            || generation
                .entry_addr
                .load(std::sync::atomic::Ordering::Acquire)
                == 0
            || generation.generated_stack_frame_bytes == 0
        {
            return None;
        }
        Some(JitDirectCallPlan {
            function_id: function.id,
            code_object_id: generation.code_object_id,
            entry_cell: std::ptr::from_ref(function_entry.as_ref()) as u64,
            tier: registered.code.native_frame_kind(),
            this_mode: if function.is_strict || function.is_arrow {
                JitDirectCallThisMode::StrictOrLexical
            } else {
                JitDirectCallThisMode::SloppyGlobal
            },
            generated_stack_frame_bytes: Some(generation.generated_stack_frame_bytes),
            param_count: function.param_count,
            register_count: function.register_count,
        })
    }

    /// Re-read one permanent function cell through the registry's cold
    /// resolver. This path does not compile or allocate; it repairs publication
    /// from already-installed generations and returns the selected generation
    /// cell, or zero when generated calls must side-exit.
    #[must_use]
    pub(crate) fn resolve_function_entry(&mut self, function_entry_addr: u64) -> u64 {
        let Some((&function_id, _)) = self
            .function_entry_cells
            .iter()
            .find(|(_, cell)| std::ptr::from_ref(cell.as_ref()) as u64 == function_entry_addr)
        else {
            return 0;
        };
        self.refresh_function_entry(function_id);
        self.function_entry_cells[&function_id].current_generation()
    }

    /// Stable machine-visible cell for the exact installed code generation.
    ///
    /// The returned address remains valid for the registry lifetime, including
    /// after invalidation and executable retirement. Native linkage must still
    /// acquire/recheck the cell because an invalidated tombstone has a zero
    /// entry address.
    #[must_use]
    #[cfg(test)]
    fn entry_cell_addr_for_entry(&self, code: &dyn JitFunctionCode) -> Option<u64> {
        if !self.is_current_for_entry(code) {
            return None;
        }
        self.entry_cells
            .get(&code.metadata().id)
            .map(|cell| std::ptr::from_ref(cell.as_ref()) as u64)
    }

    /// Unlink every installed body compiled from `function_id`: the code takes
    /// no new entries, while active entry-cell leases keep its mapping alive
    /// until their compiled frames return.
    pub(crate) fn invalidate_function(&mut self, function_id: u32) -> Vec<u32> {
        let seeds = self
            .codes
            .iter()
            .filter_map(|(&code_object_id, registered)| {
                (registered.state == CodeLifetimeState::Installed
                    && registered.code.metadata().code_block_id == function_id)
                    .then_some(code_object_id)
            })
            .collect::<Vec<_>>();
        self.invalidate_code_objects(seeds)
    }

    /// Unlink one exact installed generation while preserving every other tier
    /// and OSR body for the same function.
    ///
    /// Successful baseline feedback refresh uses this narrow operation: a new
    /// entry generation must not discard an independently installed optimizing
    /// loop body.
    pub(crate) fn invalidate_code_object(&mut self, code_object_id: u64) -> Vec<u32> {
        self.invalidate_code_objects([code_object_id])
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
    ) -> Vec<u32> {
        let published = self.epochs.entry((kind, identity)).or_insert(0);
        debug_assert!(
            current_epoch >= *published,
            "dependency epochs must not move backwards"
        );
        if current_epoch <= *published {
            return Vec::new();
        }
        *published = current_epoch;
        let seeds = self
            .codes
            .iter()
            .filter_map(|(&code_object_id, registered)| {
                (registered.state == CodeLifetimeState::Installed
                    && registered.dependencies.iter().any(|dependency| {
                        dependency.kind == kind
                            && dependency.identity == identity
                            && dependency.expected < current_epoch
                    }))
                .then_some(code_object_id)
            })
            .collect::<Vec<_>>();
        self.invalidate_code_objects(seeds)
    }

    /// Retire invalid code whose last `Arc` owner is the registry and whose
    /// unlinked entry cell has no explicit active lease.
    ///
    /// The interpreter may call this only at a native-activation epoch
    /// boundary. Generated calls deliberately avoid per-entry leases; their
    /// published outer activation pins every entry address they can still be
    /// executing until that boundary.
    ///
    /// Returns how many objects retired.
    pub(crate) fn retire_unreferenced(&mut self) -> usize {
        let before = self.codes.len();
        let entry_cells = &self.entry_cells;
        self.codes.retain(|code_object_id, registered| {
            registered.state != CodeLifetimeState::Invalid
                || Arc::strong_count(&registered.code) > 1
                || entry_cells
                    .get(code_object_id)
                    .is_some_and(|cell| !cell.can_retire())
        });
        before - self.codes.len()
    }

    /// Stable address of one generation's entry cell, including tombstones.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn entry_cell_addr(&self, code_object_id: u64) -> Option<u64> {
        self.entry_cells
            .get(&code_object_id)
            .map(|cell| std::ptr::from_ref(cell.as_ref()) as u64)
    }

    /// Snapshot every permanent entry-cell generation in deterministic
    /// code-object order.
    ///
    /// Registered executable metadata contributes lifecycle and dependencies.
    /// Once retired, the retained cell remains observable as a dependency-free
    /// tombstone with its final counters.
    #[must_use]
    pub(crate) fn generation_snapshot(&self) -> Vec<JitCodeGenerationSnapshot> {
        let mut generations = Vec::with_capacity(self.entry_cells.len());
        for (&code_object_id, cell) in &self.entry_cells {
            debug_assert_eq!(cell.code_object_id, code_object_id);
            let registered = self.codes.get(&code_object_id);
            let (entries, returns, deopts, throws, streak) = cell.generated_feedback();
            generations.push(JitCodeGenerationSnapshot {
                code_object_id,
                function_id: cell.function_id,
                tier: if cell.flags & CODE_ENTRY_OPTIMIZING_TIER == 0 {
                    NativeFrameKind::Baseline
                } else {
                    NativeFrameKind::Optimizing
                },
                lifecycle: registered
                    .map_or(CodeLifetimeState::Retired, |registered| registered.state),
                linked: cell.entry_addr.load(std::sync::atomic::Ordering::Acquire) != 0,
                active_count: cell.active_count(),
                param_count: cell.param_count,
                register_count: cell.register_count,
                generated_entries: entries,
                generated_returns: returns,
                generated_deopts: deopts,
                generated_throws: throws,
                generated_bail_streak: streak,
                dependencies: registered.map(|registered| {
                    let mut dependencies = registered.dependencies.to_vec();
                    dependencies.sort_unstable_by_key(|dependency| {
                        (
                            dependency.kind as u16,
                            dependency.identity,
                            dependency.expected,
                        )
                    });
                    dependencies
                }),
            });
        }
        generations.sort_unstable_by_key(|generation| generation.code_object_id);
        generations
    }

    /// Drain exact-generation generated-call deltas for cold tier-policy and
    /// budget reconciliation. Entry cells are retained tombstones, so feedback
    /// remains available after invalidation and executable retirement.
    pub(crate) fn take_generated_feedback(&mut self) -> Vec<GeneratedCallFeedback> {
        let mut feedback = Vec::new();
        for (&code_object_id, cell) in &self.entry_cells {
            let (entries, returns, deopts, throws, _) = cell.generated_feedback();
            let seen = self
                .generated_feedback_seen
                .entry(code_object_id)
                .or_default();
            let delta = GeneratedCallFeedback {
                function_id: cell.function_id,
                code_object_id,
                tier: if cell.flags & CODE_ENTRY_OPTIMIZING_TIER == 0 {
                    NativeFrameKind::Baseline
                } else {
                    NativeFrameKind::Optimizing
                },
                entries: entries.saturating_sub(seen.entries),
                returns: returns.saturating_sub(seen.returns),
                deopts: deopts.saturating_sub(seen.deopts),
                throws: throws.saturating_sub(seen.throws),
            };
            *seen = GeneratedFeedbackSeen {
                entries,
                returns,
                deopts,
                throws,
            };
            if delta.entries != 0 || delta.returns != 0 || delta.deopts != 0 || delta.throws != 0 {
                feedback.push(delta);
            }
        }
        feedback.sort_unstable_by_key(|entry| entry.code_object_id);
        feedback
    }

    /// Current generated-call health for one exact generation.
    pub(crate) fn generated_deopt_state(&self, code_object_id: u64) -> Option<GeneratedDeoptState> {
        let cell = self.entry_cells.get(&code_object_id)?;
        let (entries, _, deopts, _, consecutive_deopts) = cell.generated_feedback();
        let tier = if cell.flags & CODE_ENTRY_OPTIMIZING_TIER == 0 {
            NativeFrameKind::Baseline
        } else {
            NativeFrameKind::Optimizing
        };
        Some(GeneratedDeoptState {
            function_id: cell.function_id,
            tier,
            entries,
            deopts,
            consecutive_deopts,
            linked: cell.entry_addr.load(std::sync::atomic::Ordering::Acquire) != 0,
        })
    }

    /// Function identity retained for one exact generated-code generation.
    pub(crate) fn generation_function_id(&self, code_object_id: u64) -> Option<u32> {
        self.entry_cells
            .get(&code_object_id)
            .map(|cell| cell.function_id)
    }

    /// Address of the published view for [`crate::native_abi::VmThread`].
    #[must_use]
    pub(crate) fn view_addr(&self) -> u64 {
        std::ptr::addr_of!(self.view) as u64
    }

    fn dependencies_are_current(&self, dependencies: &[CodeDependency]) -> bool {
        dependencies.iter().all(|dependency| {
            dependency.expected
                == self
                    .epochs
                    .get(&(dependency.kind, dependency.identity))
                    .copied()
                    .unwrap_or(0)
        })
    }

    /// Select the best entry-capable installed generation for one function and
    /// publish it through the permanent function cell.
    fn refresh_function_entry(&mut self, function_id: u32) {
        let target = self
            .codes
            .iter()
            .filter_map(|(&code_object_id, registered)| {
                if registered.state != CodeLifetimeState::Installed
                    || registered.code.metadata().code_block_id != function_id
                    || registered.code.osr_only()
                    || !self.dependencies_are_current(&registered.dependencies)
                {
                    return None;
                }
                let cell = self.entry_cells.get(&code_object_id)?;
                if cell.entry_addr.load(std::sync::atomic::Ordering::Acquire) == 0
                    || cell.generated_stack_frame_bytes == 0
                {
                    return None;
                }
                let tier_priority =
                    u8::from(registered.code.native_frame_kind() == NativeFrameKind::Optimizing);
                Some((
                    (tier_priority, code_object_id),
                    std::ptr::from_ref(cell.as_ref()) as u64,
                ))
            })
            .max_by_key(|(priority, _)| *priority)
            .map_or(0, |(_, cell)| cell);
        let Some(function_entry) = self.function_entry_cells.get(&function_id) else {
            return;
        };
        if target == 0 {
            function_entry.clear();
        } else {
            function_entry.publish(target);
        }
    }

    /// Invalidate exact generations only. Stable function cells are refreshed
    /// after unlinking, so a remaining entry-capable tier becomes visible
    /// atomically without touching caller code.
    fn invalidate_code_objects(&mut self, seeds: impl IntoIterator<Item = u64>) -> Vec<u32> {
        let mut affected = std::collections::BTreeSet::new();
        for code_object_id in seeds {
            if let Some(registered) = self.codes.get_mut(&code_object_id)
                && registered.state == CodeLifetimeState::Installed
            {
                registered.state = CodeLifetimeState::Invalid;
                affected.insert(registered.code.metadata().code_block_id);
                if let Some(cell) = self.entry_cells.get(&code_object_id) {
                    cell.unlink();
                }
            }
        }
        for &function_id in &affected {
            self.refresh_function_entry(function_id);
        }
        affected.into_iter().collect()
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
            CodeObjectMetadata {
                id: self.id,
                code_block_id: 0,
                entry_offset: 0,
                code_size: 4,
                safepoint_count: self.records.len() as u32,
                frame_map_count: 0,
                spill_map_count: 0,
                dependency_count: self.dependencies.len() as u32,
            }
        }

        fn dependencies(&self) -> &[CodeDependency] {
            &self.dependencies
        }

        fn code_len(&self) -> usize {
            4
        }

        fn entry_addr(&self) -> Option<usize> {
            Some(0x1000 + self.id as usize * 16)
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

    #[derive(Debug)]
    struct GeneratedFakeCode {
        id: u64,
        function_id: u32,
        tier: NativeFrameKind,
        native_frame_bytes: u32,
    }

    impl JitFunctionCode for GeneratedFakeCode {
        fn metadata(&self) -> CodeObjectMetadata {
            CodeObjectMetadata {
                id: self.id,
                code_block_id: self.function_id,
                entry_offset: 0,
                code_size: 4,
                safepoint_count: 0,
                frame_map_count: 0,
                spill_map_count: 0,
                dependency_count: 0,
            }
        }

        fn native_frame_kind(&self) -> NativeFrameKind {
            self.tier
        }

        fn generated_stack_frame_bytes(&self) -> Option<u32> {
            Some(self.native_frame_bytes)
        }

        fn code_len(&self) -> usize {
            4
        }

        fn entry_addr(&self) -> Option<usize> {
            Some(0x10_0000 + self.id as usize * 16)
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
        assert!(registry.register(11, code.clone()));
        assert!(registry.is_current_for_entry(code.as_ref()));

        registry.invalidate_function(0);
        assert!(
            !registry.is_current_for_entry(code.as_ref()),
            "unlinked zero-dependency code must reject every new entry"
        );
        drop(code);
        // An explicit external owner keeps invalid code registered and
        // resolvable independently of native entry-cell leases.
        assert_eq!(registry.retire_unreferenced(), 0);
        let view = unsafe { *(registry.view_addr() as *const CodeRegistryView) };
        assert!(unsafe { view.resolve(11, 1) }.is_some());

        drop(anchor);
        assert_eq!(registry.retire_unreferenced(), 1);
        assert!(unsafe { view.resolve(11, 1) }.is_none());
    }

    #[test]
    fn production_entry_cell_unlinks_before_code_retires_and_remains_a_tombstone() {
        let mut registry = JitCodeRegistry::new_boxed();
        let code = fake_code(13, Vec::new());
        assert!(registry.register_generation(13, code.clone(), 2, 9));
        let cell_addr = registry.entry_cell_addr(13).expect("entry cell installed");
        assert_eq!(
            registry.entry_cell_addr_for_entry(code.as_ref()),
            Some(cell_addr)
        );
        // SAFETY: entry cells are boxed and retained for the registry lifetime.
        let cell = unsafe { &*(cell_addr as *const CodeEntryCell) };
        assert_eq!(cell.code_object_id, 13);
        assert_eq!(cell.param_count, 2);
        assert_eq!(cell.register_count, 9);
        assert!(cell.try_acquire().is_some());

        registry.invalidate_function(0);
        assert_eq!(registry.entry_cell_addr_for_entry(code.as_ref()), None);
        assert!(cell.try_acquire().is_none(), "invalidation unlinks first");
        assert!(cell.can_retire());
        drop(code);
        assert_eq!(registry.retire_unreferenced(), 1);
        assert_eq!(registry.entry_cell_addr(13), Some(cell_addr));
        // SAFETY: retirement deliberately retains the tombstone cell.
        let tombstone = unsafe { &*(cell_addr as *const CodeEntryCell) };
        assert_eq!(
            tombstone
                .entry_addr
                .load(std::sync::atomic::Ordering::Acquire),
            0
        );
    }

    #[test]
    fn stable_function_entry_switches_tiers_without_invalidating_callers() {
        let mut registry = JitCodeRegistry::new_boxed();
        let baseline: Arc<dyn JitFunctionCode> = Arc::new(GeneratedFakeCode {
            id: 101,
            function_id: 7,
            tier: NativeFrameKind::Baseline,
            native_frame_bytes: 64,
        });
        let optimizing: Arc<dyn JitFunctionCode> = Arc::new(GeneratedFakeCode {
            id: 102,
            function_id: 7,
            tier: NativeFrameKind::Optimizing,
            native_frame_bytes: 96,
        });
        let caller: Arc<dyn JitFunctionCode> = Arc::new(GeneratedFakeCode {
            id: 201,
            function_id: 8,
            tier: NativeFrameKind::Baseline,
            native_frame_bytes: 64,
        });

        assert!(registry.register_generation(101, baseline, 2, 9));
        assert!(registry.register_generation(201, caller, 2, 12));
        let stable_addr = std::ptr::from_ref(registry.function_entry_cells[&7].as_ref()) as u64;
        assert_eq!(
            registry.function_entry_cells[&7].current_generation(),
            registry.entry_cell_addr(101).unwrap()
        );

        assert!(registry.register_generation(102, optimizing, 2, 9));
        assert_eq!(
            std::ptr::from_ref(registry.function_entry_cells[&7].as_ref()) as u64,
            stable_addr,
            "promotion must preserve the caller-baked function-cell address"
        );
        assert_eq!(
            registry.function_entry_cells[&7].current_generation(),
            registry.entry_cell_addr(102).unwrap(),
            "optimizing tier becomes the published target"
        );

        assert_eq!(registry.invalidate_code_object(102), vec![7]);
        assert_eq!(
            registry.function_entry_cells[&7].current_generation(),
            registry.entry_cell_addr(101).unwrap(),
            "invalidating the optimizer republishes the installed baseline"
        );
        assert_eq!(
            registry.codes[&201].state,
            CodeLifetimeState::Installed,
            "callee tier changes must not invalidate generated callers"
        );

        let optimizing_refresh: Arc<dyn JitFunctionCode> = Arc::new(GeneratedFakeCode {
            id: 103,
            function_id: 7,
            tier: NativeFrameKind::Optimizing,
            native_frame_bytes: 96,
        });
        assert!(registry.register_generation(103, optimizing_refresh, 2, 9));
        assert_eq!(registry.invalidate_code_object(101), vec![7]);
        assert_eq!(
            registry.function_entry_cells[&7].current_generation(),
            registry.entry_cell_addr(103).unwrap(),
            "refreshing baseline must preserve the independent optimizing generation"
        );
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
                .is_current_for_entry(interp.jit_code_registry.codes[&21].code.as_ref())
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
                .is_current_for_entry(interp.jit_code_registry.codes[&24].code.as_ref())
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
        let mut stack = crate::activation_stack::ActivationStack::new();

        assert!(
            interp
                .set_prototype_value_proxy_aware(
                    &mut stack,
                    &context,
                    &target_value,
                    &prototype_value,
                )
                .expect("ordinary prototype mutation")
        );
        assert_eq!(interp.shape_epoch(), 1);
        assert_eq!(
            interp.jit_code_registry.codes[&41].state,
            CodeLifetimeState::Invalid
        );

        assert!(
            interp
                .set_prototype_value_proxy_aware(
                    &mut stack,
                    &context,
                    &target_value,
                    &prototype_value,
                )
                .expect("same prototype is accepted")
        );
        assert_eq!(interp.shape_epoch(), 1);
    }
}
