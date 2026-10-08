//! Isolate-owned registry of installed JIT code objects.
//!
//! The registry maps unique code-object ids to installed
//! [`crate::jit::JitFunctionCode`] objects, snapshots their isolate-state
//! dependencies, and publishes one stable [`CodeRegistryView`] so native code
//! can resolve active helper safepoints and exact suspended JS return addresses
//! in the expected retained code object. Entry, root and source metadata share
//! that same code-object lifetime.
//!
//! # Contents
//! - [`JitCodeRegistry`] — boxed, address-stable registry cell.
//! - Stable per-function entry dispatch plus per-generation
//!   [`crate::native_abi::CodeEntryCell`] installation, unlinking, and
//!   tombstone retention.
//! - Dependency registration, exact-epoch entry consistency, and monotonic
//!   invalidation.
//! - `resolve_jit_registry_safepoint` — the machine-visible resolver behind
//!   the published view, plus exact return-PC lookup in one expected mapping.
//!   Borrowed records also retain complete inline source recipes.
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
//! - Existing GeneratedCodeBytes leases bound mappings, owned generation
//!   payloads and permanent entry-cell tombstones. Directory/bucket and
//!   allocator bookkeeping are excluded; this is not a total process RSS cap.
//! - Existing GeneratedCodeBytes leases bound all retained mappings, including
//!   active invalid bodies, to the isolate cap and the host account limit.
//! - Each function has one current generation per native tier. A successful
//!   install unlinks superseded same-tier bodies only after admission; active
//!   mappings retain their root and deopt records until retirement.
//! - Dependency epochs are isolate-local and monotonic. Install and entry
//!   selection require `expected == current`; invalidation marks Installed
//!   code only when `expected < current` for the same `(kind, identity)`.
//! - Invalid code remains available to safepoint resolution until its last
//!   external anchor drops and the interpreter reaches a native-activation
//!   retirement epoch before [`JitCodeRegistry::retire_unreferenced`] removes
//!   it. A published frame's code id cannot stand in for that epoch: a frame
//!   resumed in the interpreter after a deopt clears its id while its
//!   generated machine frame still unwinds through the exit path.
//!   Safepoint resolution therefore does not apply the entry check.
//! - Cold generation snapshots derive callable-entry offsets from the one
//!   retained code mapping and current selection from the permanent function
//!   cell; compile-trigger labels do not describe callable capability.
//! - Function publication points only into registry-owned generation cells,
//!   including retained tombstones. Reading the current target never scans
//!   generation history or consults a second address-to-generation index.
//! - Generated callers retain only stable function-cell addresses. Publishing a
//!   new generation never invalidates or recompiles dependent callers.
//! - A code object's immutable spliced-function list covers actual inlines,
//!   including ones without safepoints or exits. A deopt unlinks the source
//!   function's optimizing generations and every optimizing caller that
//!   spliced it; its baseline generation stays installed, and its stable cell
//!   selects the interpreter until retraining completes (V8 keeps baseline
//!   code across a deopt the same way).
//! - Invalidating a generation unlinks its entry cell before executable
//!   retirement. The cell address remains valid and is never reused.
//!
//! # See also
//! - [`crate::native_abi::CodeRegistryView`] — the published lookup surface.

use crate::jit::{
    JitCodeGenerationSnapshot, JitDirectCallPlan, JitDirectCallThisMode, JitFunctionCode,
};
use crate::native_abi::{
    CODE_ENTRY_HAS_SAFEPOINTS, CODE_ENTRY_OPTIMIZING_TIER, CodeDependency, CodeDependencyKind,
    CodeEntryCell, CodeLifetimeState, CodeRegistryView, FunctionEntryCell, NativeFrameKind,
    SafepointId, SafepointRecord,
};
use std::sync::Arc;

/// Admission failure at the canonical code-object installation boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JitInstallError {
    InvalidCode,
    ResourceBudget { required_bytes: u64 },
}

/// One registered code object with its lifecycle state.
struct RegisteredCode {
    code: Arc<dyn JitFunctionCode>,
    dependencies: Box<[CodeDependency]>,
    state: CodeLifetimeState,
    /// Shapes and validity cells embedded by this generation. Invalidated
    /// code may still execute, so these live until physical retirement.
    roots: Box<[crate::jit_roots::CompilationRoot]>,
    /// Exact `GeneratedCodeBytes` charge for the executable mapping and its
    /// owned metadata. Released when the retired object is physically
    /// dropped from the registry.
    generated_code_lease: otter_resource::ResourceLease,
}

/// The generation entry and its permanent metadata charge share one owner.
/// Tombstone cells stay address-stable and charged after executable retirement.
struct RegisteredEntry {
    cell: Box<CodeEntryCell>,
    generated_code_lease: otter_resource::ResourceLease,
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
}

/// Current direct-call health for one exact generated code object.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GeneratedDeoptState {
    pub(crate) function_id: u32,
    pub(crate) tier: NativeFrameKind,
    pub(crate) linked: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct GeneratedFeedbackSeen {
    entries: u64,
    returns: u64,
    deopts: u64,
}

/// Isolate-owned installed-code registry behind a stable published view.
pub struct JitCodeRegistry {
    /// Published C-layout lookup surface; `context` names this registry.
    view: CodeRegistryView,
    /// Ledger charged for every installed code object's retained bytes.
    /// Defaults to a private unlimited account; the runtime installs its
    /// shared account at construction.
    account: otter_resource::ResourceAccount,
    /// Installed code objects by unique code-object id.
    codes: rustc_hash::FxHashMap<u64, RegisteredCode>,
    /// Address-stable entry cells by code generation. Cells are tombstoned on
    /// invalidation and intentionally survive executable retirement so a baked
    /// pointer can never observe freed or repurposed metadata.
    entry_cells: rustc_hash::FxHashMap<u64, RegisteredEntry>,
    /// Sum of every `codes` and `entry_cells` lease amount, maintained at each
    /// insertion and removal so admission never walks the generation maps.
    retained_bytes: u64,
    /// Permanent generated-call linkage cells by bytecode function. Callers
    /// bake these addresses once; tier publication switches only the contained
    /// generation-cell pointer.
    function_entry_cells: rustc_hash::FxHashMap<u32, Box<FunctionEntryCell>>,
    /// Functions retraining after a deopt: their stable cells select the
    /// interpreter while their baseline generation stays installed.
    retraining: rustc_hash::FxHashSet<u32>,
    /// Indexed linkage addresses published through the stable registry view.
    function_entries: Vec<u64>,
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
                function_entries: 0,
                function_entry_count: 0,
                resolve_return_pc: resolve_jit_registry_return_pc as *const () as u64,
            },
            account: otter_resource::ResourceAccount::default(),
            codes: rustc_hash::FxHashMap::default(),
            entry_cells: rustc_hash::FxHashMap::default(),
            retained_bytes: 0,
            function_entry_cells: rustc_hash::FxHashMap::default(),
            retraining: rustc_hash::FxHashSet::default(),
            function_entries: Vec::new(),
            generated_feedback_seen: rustc_hash::FxHashMap::default(),
            epochs: rustc_hash::FxHashMap::default(),
        });
        registry.view.context = std::ptr::addr_of!(*registry) as u64;
        registry
    }

    /// Publish permanent linkage for a bytecode function at module linking.
    ///
    /// The cell is built from the function's admitted header alone, so
    /// linking never verifies or builds a body; the first call does.
    pub(crate) fn link_function(&mut self, function: &otter_bytecode::Function, realm_id: u32) {
        self.ensure_function_entry(
            function.id,
            function.param_count,
            function
                .register_count()
                .expect("admitted function header has a register window"),
            crate::executable::bytecode_call_flags(function),
            realm_id,
        );
    }

    fn ensure_function_entry(
        &mut self,
        function_id: u32,
        param_count: u16,
        register_count: u16,
        call_flags: u32,
        realm_id: u32,
    ) {
        let cell = self
            .function_entry_cells
            .entry(function_id)
            .or_insert_with(|| {
                FunctionEntryCell::new(
                    function_id,
                    param_count,
                    register_count,
                    call_flags,
                    realm_id,
                )
            });
        assert_eq!(
            cell.param_count, param_count,
            "linked function parameter layout is immutable"
        );
        assert_eq!(
            cell.register_count, register_count,
            "linked function register layout is immutable"
        );
        let index = function_id as usize;
        if self.function_entries.len() <= index {
            self.function_entries.resize(index + 1, 0);
        }
        self.function_entries[index] = std::ptr::from_ref(cell.as_ref()) as u64;
        self.view.function_entries = self.function_entries.as_ptr() as u64;
        self.view.function_entry_count = self.function_entries.len() as u64;
    }

    /// Install the ledger charged for installed code objects' retained bytes.
    pub(crate) fn set_account(&mut self, account: otter_resource::ResourceAccount) {
        self.account = account;
    }

    /// Register one current code object under its unique id.
    ///
    /// Declines without installing when the metadata identity or code
    /// size is invalid, the declared count differs from the dependency slice,
    /// or any dependency is not exactly current. The code object owns the
    /// declaration surface; the registry snapshots it so later invalidation
    /// never depends on a virtual call into mutable compiler state.
    #[cfg(test)]
    pub(crate) fn register(
        &mut self,
        code_object_id: u64,
        code: Arc<dyn JitFunctionCode>,
    ) -> Result<(), JitInstallError> {
        self.register_inner(code_object_id, code, None, Box::new([]))
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
        optimizing_work_target: Option<u64>,
        roots: Box<[crate::jit_roots::CompilationRoot]>,
    ) -> Result<(), JitInstallError> {
        let metadata = code.metadata();
        if metadata.id != expected_code_object_id || metadata.code_block_id != function.id {
            return Err(JitInstallError::InvalidCode);
        }
        self.register_generation(
            metadata.id,
            code,
            function.param_count,
            function.register_count,
            optimizing_work_target,
            roots,
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
        optimizing_work_target: Option<u64>,
        roots: Box<[crate::jit_roots::CompilationRoot]>,
    ) -> Result<(), JitInstallError> {
        let Some(entry_addr) = code.entry_addr() else {
            return Err(JitInstallError::InvalidCode);
        };
        if entry_addr == 0 {
            return Err(JitInstallError::InvalidCode);
        }
        let metadata = code.metadata();
        let tier = code.native_frame_kind();
        let mut flags = 0;
        if code.safepoint_count() != 0 {
            flags |= CODE_ENTRY_HAS_SAFEPOINTS;
        }
        if code.native_frame_kind() == NativeFrameKind::Optimizing {
            flags |= CODE_ENTRY_OPTIMIZING_TIER;
        }
        if param_count > register_count {
            return Err(JitInstallError::InvalidCode);
        }
        // A body without a call entry is reached only through classification,
        // which runs a suspendable function's interpreter destination.
        let call_entry = code
            .call_entry_addr()
            .unwrap_or(crate::native_abi::call_generic_entry as *const () as usize);
        let entry_cell = Box::new(CodeEntryCell::new(
            call_entry,
            code_object_id,
            metadata.code_block_id,
            register_count,
            flags,
            optimizing_work_target,
        ));
        self.register_inner(
            code_object_id,
            code,
            Some((entry_cell, param_count, register_count)),
            roots,
        )?;
        // The VM owns one whole-body map slot per function and tier, including
        // OSR bodies. Admission failure must preserve its previous generation;
        // after success that superseded body can no longer be selected or
        // resurrected by a later publication refresh. Active extents retain its
        // invalid mapping through the usual lease/root-record boundary.
        let superseded = self
            .codes
            .iter()
            .filter_map(|(&id, registered)| {
                (id != code_object_id
                    && registered.state == CodeLifetimeState::Installed
                    && registered.code.metadata().code_block_id == metadata.code_block_id
                    && registered.code.native_frame_kind() == tier)
                    .then_some(id)
            })
            .collect::<Vec<_>>();
        self.invalidate_code_objects(superseded);
        Ok(())
    }

    /// Exact requested mapping and owned payload bytes for one finalized
    /// generation, including its persistent entry cell. Shared Arc/GC targets,
    /// directory/bucket storage and allocator bookkeeping have other owners.
    pub(crate) fn retained_admission_bytes(
        code: &dyn JitFunctionCode,
        roots: &[crate::jit_roots::CompilationRoot],
    ) -> u64 {
        Self::retained_payload_bytes(code, roots)
            .saturating_add(std::mem::size_of::<CodeEntryCell>() as u64)
    }

    fn retained_payload_bytes(
        code: &dyn JitFunctionCode,
        roots: &[crate::jit_roots::CompilationRoot],
    ) -> u64 {
        code.retained_bytes()
            .saturating_add(std::mem::size_of_val(code.dependencies()) as u64)
            .saturating_add(std::mem::size_of_val(roots) as u64)
    }

    fn retained_generated_bytes(&self) -> u64 {
        debug_assert_eq!(
            self.retained_bytes,
            self.codes
                .values()
                .map(|registered| registered.generated_code_lease.amount())
                .chain(
                    self.entry_cells
                        .values()
                        .map(|entry| entry.generated_code_lease.amount())
                )
                .sum::<u64>()
        );
        self.retained_bytes
    }

    /// Remaining admission headroom from the existing physical mapping leases.
    /// Invalid active generations remain charged until physical retirement.
    /// The host account may impose a lower limit than the isolate cap.
    pub(crate) fn available_code_bytes(&self) -> u64 {
        let retained = self.retained_generated_bytes();
        let isolate_available =
            crate::tier_policy::JIT_CODE_RESOURCE_LIMIT_BYTES.saturating_sub(retained);
        let snapshot = self.account.snapshot();
        let account = snapshot.get(otter_resource::ResourceClass::GeneratedCodeBytes);
        let host_available = account
            .limit()
            .map_or(u64::MAX, |limit| limit.saturating_sub(account.current()));
        isolate_available.min(host_available)
    }

    fn register_inner(
        &mut self,
        code_object_id: u64,
        code: Arc<dyn JitFunctionCode>,
        entry_cell: Option<(Box<CodeEntryCell>, u16, u16)>,
        roots: Box<[crate::jit_roots::CompilationRoot]>,
    ) -> Result<(), JitInstallError> {
        debug_assert_ne!(code_object_id, 0);
        debug_assert_eq!(code.metadata().id, code_object_id);
        let metadata = code.metadata();
        let dependencies: Box<[CodeDependency]> = code.dependencies().into();
        let spliced = code.spliced_functions();
        if code_object_id == 0
            || metadata.id != code_object_id
            || metadata.code_size == 0
            || metadata.dependency_count as usize != dependencies.len()
            || !self.dependencies_are_current(&dependencies)
            || spliced.binary_search(&metadata.code_block_id).is_ok()
            || !spliced.windows(2).all(|pair| pair[0] < pair[1])
            || !crate::native_abi::valid_return_sites(code.as_ref())
        {
            return Err(JitInstallError::InvalidCode);
        }
        if self.codes.contains_key(&code_object_id)
            || self.entry_cells.contains_key(&code_object_id)
        {
            debug_assert!(false, "code-object ids are never reused");
            return Err(JitInstallError::InvalidCode);
        }
        // Mapping/metadata and the address-stable entry cell have separate
        // physical lifetimes. Reserve both before publication; any failure
        // drops the first lease and leaves the previous generation installed.
        let payload_bytes = Self::retained_payload_bytes(code.as_ref(), &roots);
        let entry_bytes = if entry_cell.is_some() {
            std::mem::size_of::<CodeEntryCell>() as u64
        } else {
            0
        };
        let required_bytes = payload_bytes.saturating_add(entry_bytes);
        if required_bytes
            > crate::tier_policy::JIT_CODE_RESOURCE_LIMIT_BYTES
                .saturating_sub(self.retained_generated_bytes())
        {
            return Err(JitInstallError::ResourceBudget { required_bytes });
        }
        let generated_code_lease = self
            .account
            .reserve_exact(
                otter_resource::ResourceClass::GeneratedCodeBytes,
                payload_bytes,
            )
            .map_err(|_| JitInstallError::ResourceBudget { required_bytes })?;
        let entry_lease = if entry_cell.is_some() {
            Some(
                self.account
                    .reserve_exact(
                        otter_resource::ResourceClass::GeneratedCodeBytes,
                        entry_bytes,
                    )
                    .map_err(|_| JitInstallError::ResourceBudget { required_bytes })?,
            )
        } else {
            None
        };
        let replaced = self.codes.insert(
            code_object_id,
            RegisteredCode {
                code,
                dependencies,
                state: CodeLifetimeState::Installed,
                roots,
                generated_code_lease,
            },
        );
        debug_assert!(replaced.is_none(), "code-object ids are never reused");
        self.retained_bytes += payload_bytes;
        if let Some((entry_cell, param_count, register_count)) = entry_cell {
            let function_id = entry_cell.native_frame_header.function_id;
            self.ensure_function_entry(function_id, param_count, register_count, 0, 0);
            let function_entry = &self.function_entry_cells[&function_id];
            if function_entry.param_count != param_count
                || function_entry.register_count != register_count
            {
                debug_assert!(
                    false,
                    "function entry layout cannot change across generations"
                );
                self.codes.remove(&code_object_id);
                self.retained_bytes -= payload_bytes;
                return Err(JitInstallError::InvalidCode);
            }
            let replaced = self.entry_cells.insert(
                code_object_id,
                RegisteredEntry {
                    cell: entry_cell,
                    generated_code_lease: entry_lease
                        .expect("compiled generation owns entry lease"),
                },
            );
            debug_assert!(replaced.is_none(), "entry-cell ids are never reused");
            self.retained_bytes += entry_bytes;
            self.refresh_function_entry(function_id);
        }
        Ok(())
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
                && self.entry_cells.get(&metadata.id).is_none_or(|entry| {
                    let cell = &entry.cell;
                    cell.entry_addr.load(std::sync::atomic::Ordering::Acquire) != 0
                })
        })
    }

    /// Whether the exact generation currently executing a native loop remains
    /// installed. Invalidated mappings remain readable for active deopt, but
    /// cannot continue indefinitely through their spliced bodies.
    pub(crate) fn is_current_generation(&self, code_object_id: u64) -> bool {
        self.codes
            .get(&code_object_id)
            .is_some_and(|registered| self.is_current_for_entry(registered.code.as_ref()))
    }

    /// Store the policy-owned absolute source-work wakeup without charging
    /// entries or restarting already observed work.
    pub(crate) fn defer_generated_tiering(&self, function_id: u32, target: Option<u64>) {
        if let Some((_, generation)) = self.published_function_entry(function_id) {
            match target {
                Some(target) => {
                    generation.generated_tiering_work_target.set(target);
                    generation.generated_tiering_enabled.set(u32::from(
                        generation.code_object_id != 0
                            && generation.flags & CODE_ENTRY_OPTIMIZING_TIER == 0,
                    ));
                }
                None => {
                    generation.generated_tiering_work_target.set(u64::MAX);
                    generation.generated_tiering_enabled.set(0);
                }
            }
        }
    }

    /// A cached compile outcome makes repeated requests from this generation
    /// redundant. A future replacement owns its own fresh eligibility bit.
    pub(crate) fn suppress_generated_tiering(&self, function_id: u32) {
        if let Some((_, generation)) = self.published_function_entry(function_id) {
            generation.generated_tiering_work_target.set(u64::MAX);
            generation.generated_tiering_enabled.set(0);
        }
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
        let (function_entry, generation) = self.published_function_entry(function.id)?;
        if generation.code_object_id != 0 {
            let registered = self.codes.get(&generation.code_object_id)?;
            if registered.state != CodeLifetimeState::Installed
                || !self.dependencies_are_current(&registered.dependencies)
            {
                return None;
            }
        }
        if generation
            .entry_addr
            .load(std::sync::atomic::Ordering::Acquire)
            == 0
        {
            return None;
        }
        Some(JitDirectCallPlan {
            function_id: function.id,
            code_object_id: generation.code_object_id,
            entry_cell: std::ptr::from_ref(function_entry) as u64,
            tier: generation.native_frame_header.kind,
            // An unobservable binding needs no OrdinaryCallBindThis.
            this_mode: if function.is_strict || function.is_arrow || !function.observes_this {
                JitDirectCallThisMode::StrictOrLexical
            } else {
                JitDirectCallThisMode::SloppyGlobal
            },
            is_derived_constructor: function.is_derived_constructor,
            call_flags: function.call_flags(),
            callee_cell: 0,
        })
    }

    /// Read the registry-owned publication without scanning generation history.
    /// The publication itself is the sole authority; no reverse-address index or
    /// separately cached generation id participates in target selection.
    fn published_function_entry(
        &self,
        function_id: u32,
    ) -> Option<(&FunctionEntryCell, &CodeEntryCell)> {
        let function = self.function_entry_cells.get(&function_id)?;
        let address = function.current_generation();
        if address == 0 {
            return None;
        }
        // SAFETY: only this registry publishes its boxed CodeEntryCell addresses
        // into its private function cells. Generation cells, including unlinked
        // tombstones, are retained for the entire registry lifetime. The single
        // mutator cannot change publication while this shared borrow is live.
        let generation = unsafe { &*(address as *const CodeEntryCell) };
        debug_assert_eq!(generation.native_frame_header.function_id, function_id);
        Some((function, generation))
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
            .map(|entry| std::ptr::from_ref(entry.cell.as_ref()) as u64)
    }

    /// Unlink installed bodies compiled from or actually splicing `function_id`.
    /// Ordinary stable function-cell callers remain installed and follow its
    /// replacement destination. Active frames keep invalidated mappings alive
    /// until their compiled extents return.
    pub(crate) fn invalidate_function(&mut self, function_id: u32) -> Vec<u32> {
        let seeds = self
            .codes
            .iter()
            .filter_map(|(&code_object_id, registered)| {
                (registered.state == CodeLifetimeState::Installed
                    && (registered.code.metadata().code_block_id == function_id
                        || registered
                            .code
                            .spliced_functions()
                            .binary_search(&function_id)
                            .is_ok()))
                .then_some(code_object_id)
            })
            .collect::<Vec<_>>();
        self.invalidate_code_objects(seeds)
    }

    /// Unlink the optimizing generations compiled from `function_id` and every
    /// optimizing caller that spliced it, for a deopt's retraining. Baseline
    /// generations, including baseline callers' guarded leaf splices, stay.
    pub(crate) fn invalidate_optimizing_for_retraining(&mut self, function_id: u32) -> Vec<u32> {
        let seeds = self
            .codes
            .iter()
            .filter_map(|(&code_object_id, registered)| {
                (registered.state == CodeLifetimeState::Installed
                    && registered.code.native_frame_kind() == NativeFrameKind::Optimizing
                    && (registered.code.metadata().code_block_id == function_id
                        || registered
                            .code
                            .spliced_functions()
                            .binary_search(&function_id)
                            .is_ok()))
                .then_some(code_object_id)
            })
            .collect::<Vec<_>>();
        self.invalidate_code_objects(seeds)
    }

    /// Route `function_id`'s stable cell to the interpreter while it retrains,
    /// or back to its best installed generation once retraining completes.
    pub(crate) fn set_retraining(&mut self, function_id: u32, retraining: bool) {
        let changed = if retraining {
            self.retraining.insert(function_id)
        } else {
            self.retraining.remove(&function_id)
        };
        if changed {
            self.refresh_function_entry(function_id);
        }
    }

    /// Unlink every installed generation. Chunk reclamation uses this
    /// conservative boundary because an installed caller may have inlined a
    /// function from the retiring chunk even when its own id lies elsewhere.
    pub(crate) fn invalidate_all(&mut self) -> Vec<u32> {
        let seeds = self
            .codes
            .iter()
            .filter_map(|(&code_object_id, registered)| {
                (registered.state == CodeLifetimeState::Installed).then_some(code_object_id)
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

    /// Visit every hidden class a registered (not yet retired) code object
    /// embeds, as strong roots.
    pub(crate) fn trace_retained_roots(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        for registered in self.codes.values() {
            for shape in &*registered.roots {
                shape.trace(visitor);
            }
        }
    }

    /// Remove invalid generations no owner still needs: no `Arc` beyond the
    /// registry's and no entry cell with a live lease. Callers run this only
    /// at a native-activation retirement epoch, when no published frame can
    /// still return into or enter an unleased generation.
    ///
    /// Returns how many objects retired.
    pub(crate) fn retire_unreferenced(&mut self) -> usize {
        let before = self.codes.len();
        let entry_cells = &self.entry_cells;
        let retained_bytes = &mut self.retained_bytes;
        self.codes.retain(|code_object_id, registered| {
            let keep = registered.state != CodeLifetimeState::Invalid
                || Arc::strong_count(&registered.code) > 1
                || entry_cells
                    .get(code_object_id)
                    .is_some_and(|entry| !entry.cell.can_retire());
            if !keep {
                *retained_bytes -= registered.generated_code_lease.amount();
            }
            keep
        });
        before - self.codes.len()
    }

    /// Stable address of one generation's entry cell, including tombstones.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn entry_cell_addr(&self, code_object_id: u64) -> Option<u64> {
        self.entry_cells
            .get(&code_object_id)
            .map(|entry| std::ptr::from_ref(entry.cell.as_ref()) as u64)
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
        for (&code_object_id, entry) in &self.entry_cells {
            let cell = &entry.cell;
            debug_assert_eq!(cell.code_object_id, code_object_id);
            let function_id = cell.native_frame_header.function_id;
            let function_entry = self
                .function_entry_cells
                .get(&function_id)
                .expect("every generation retains its permanent function cell");
            let registered = self.codes.get(&code_object_id);
            let (entries, returns, deopts) = cell.generated_feedback();
            generations.push(JitCodeGenerationSnapshot {
                code_object_id,
                function_id,
                tier: if cell.flags & CODE_ENTRY_OPTIMIZING_TIER == 0 {
                    NativeFrameKind::Baseline
                } else {
                    NativeFrameKind::Optimizing
                },
                lifecycle: registered
                    .map_or(CodeLifetimeState::Retired, |registered| registered.state),
                linked: cell.entry_addr.load(std::sync::atomic::Ordering::Acquire) != 0,
                current_entry: function_entry.current_generation()
                    == std::ptr::from_ref(cell.as_ref()) as u64,
                call_entry_offset: registered
                    .and_then(|registered| retained_call_entry_offset(registered.code.as_ref())),
                active_count: cell.active_count(),
                param_count: function_entry.param_count,
                register_count: function_entry.register_count,
                generated_entries: entries,
                generated_returns: returns,
                generated_deopts: deopts,
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

    /// Calls of `function_id` its generated generations and its interpreter
    /// route have counted, drained or not.
    pub(crate) fn counted_entries(&self, function_id: u32) -> u64 {
        let routed = self
            .function_entry_cells
            .get(&function_id)
            .map_or(0, |function| function.interpreter_destination().generated_feedback().0);
        self.entry_cells
            .values()
            .map(|entry| entry.cell.as_ref())
            .filter(|cell| cell.native_frame_header.function_id == function_id)
            .map(|cell| cell.generated_feedback().0)
            .fold(routed, u64::saturating_add)
    }

    /// Drain exact-generation generated-call deltas for cold tier-policy and
    /// budget reconciliation. Entry cells are retained tombstones, so feedback
    /// remains available after invalidation and executable retirement.
    pub(crate) fn take_generated_feedback(&mut self) -> Vec<GeneratedCallFeedback> {
        let mut feedback = Vec::new();
        for cell in self
            .entry_cells
            .values()
            .map(|entry| entry.cell.as_ref())
            .chain(
                self.function_entry_cells
                    .values()
                    .map(|function| function.interpreter_destination()),
            )
        {
            let code_object_id = cell.code_object_id;
            let (entries, returns, deopts) = cell.generated_feedback();
            let seen = self
                .generated_feedback_seen
                .entry(std::ptr::from_ref(cell) as u64)
                .or_default();
            let delta = GeneratedCallFeedback {
                function_id: cell.native_frame_header.function_id,
                code_object_id,
                tier: cell.native_frame_header.kind,
                entries: entries.saturating_sub(seen.entries),
                returns: returns.saturating_sub(seen.returns),
                deopts: deopts.saturating_sub(seen.deopts),
            };
            *seen = GeneratedFeedbackSeen {
                entries,
                returns,
                deopts,
            };
            if delta.entries != 0 || delta.returns != 0 || delta.deopts != 0 {
                feedback.push(delta);
            }
        }
        feedback.sort_unstable_by_key(|entry| (entry.code_object_id, entry.function_id));
        feedback
    }

    /// Current generated-call health for one exact generation.
    /// The generation's state as it takes one side exit, counted in its
    /// feedback.
    pub(crate) fn generated_deopt_state(
        &self,
        code_object_id: u64,
        function_id: u32,
        frame_kind: NativeFrameKind,
    ) -> Option<GeneratedDeoptState> {
        let cell = &self.entry_cells.get(&code_object_id)?.cell;
        let tier = if cell.flags & CODE_ENTRY_OPTIMIZING_TIER == 0 {
            NativeFrameKind::Baseline
        } else {
            NativeFrameKind::Optimizing
        };
        if cell.native_frame_header.function_id != function_id || tier != frame_kind {
            return None;
        }
        cell.generated_deopts
            .set(cell.generated_deopts.get().saturating_add(1));
        Some(GeneratedDeoptState {
            function_id: cell.native_frame_header.function_id,
            tier,
            linked: cell.entry_addr.load(std::sync::atomic::Ordering::Acquire) != 0,
        })
    }

    /// Function identity retained for one exact generated-code generation.
    /// Notify the exact retained generation after validating its completion.
    pub(crate) fn note_completion(
        &self,
        code_object_id: u64,
        status: crate::native_abi::NativeResultStatus,
    ) {
        if let Some(registered) = self.codes.get(&code_object_id) {
            registered.code.note_completion(status);
        }
    }

    pub(crate) fn generation_function_id(&self, code_object_id: u64) -> Option<u32> {
        self.codes
            .get(&code_object_id)
            .map(|registered| registered.code.metadata().code_block_id)
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
        if self.retraining.contains(&function_id) {
            if let Some(function_entry) = self.function_entry_cells.get(&function_id) {
                function_entry.restore_interpreter();
            }
            return;
        }
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
                let cell = &self.entry_cells.get(&code_object_id)?.cell;
                if cell.entry_addr.load(std::sync::atomic::Ordering::Acquire) == 0 {
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
            function_entry.restore_interpreter();
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
                if let Some(entry) = self.entry_cells.get(&code_object_id) {
                    entry.cell.unlink();
                }
            }
        }
        for &function_id in &affected {
            self.refresh_function_entry(function_id);
        }
        affected.into_iter().collect()
    }

    /// Resolve immutable source/root metadata retained by an active generation.
    pub(crate) fn safepoint_record(
        &self,
        code_object_id: u64,
        safepoint_id: SafepointId,
    ) -> Option<&SafepointRecord> {
        // Invalid code still resolves: active frames keep rooting until retirement.
        self.codes
            .get(&code_object_id)
            .and_then(|registered| registered.code.safepoint_record(safepoint_id))
    }

    /// Resolve exactly this generation's genuine machine return address.
    /// Invalid active generations retain the same mapping and table until retirement.
    pub(crate) fn return_pc_record(
        &self,
        code_object_id: u64,
        return_pc: u64,
    ) -> Option<&SafepointRecord> {
        let code = self.codes.get(&code_object_id)?.code.as_ref();
        crate::native_abi::return_pc_record(code, return_pc)
    }

    fn resolve(&self, code_object_id: u64, safepoint_id: SafepointId) -> *const SafepointRecord {
        self.safepoint_record(code_object_id, safepoint_id)
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

/// Exact expected-generation return resolver published by the owning isolate.
/// The installed Arc retains its mapping and record addresses through this call.
unsafe extern "C" fn resolve_jit_registry_return_pc(
    context: u64,
    code_object_id: u64,
    return_pc: u64,
) -> *const SafepointRecord {
    if context == 0 {
        return std::ptr::null();
    }
    let registry = unsafe { &*(context as *const JitCodeRegistry) };
    registry
        .return_pc_record(code_object_id, return_pc)
        .map_or(std::ptr::null(), std::ptr::from_ref)
}

/// Callable capability belongs to the retained code, rather than its compile
/// trigger or the entry-cell fallback used by a suspendable interpreter body.
fn retained_call_entry_offset(code: &dyn JitFunctionCode) -> Option<u32> {
    let base = code.native_code_address()?;
    let entry = u64::try_from(code.call_entry_addr()?).ok()?;
    let offset = entry.checked_sub(base)?;
    if offset >= u64::try_from(code.code_len()).ok()? {
        return None;
    }
    u32::try_from(offset).ok()
}

#[cfg(test)]
#[path = "jit_registry/entry_capability_tests.rs"]
mod entry_capability_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_abi::{
        ARRAY_INDEX_ACCESSOR_PROTECTOR_IDENTITY, CodeObjectMetadata, NO_FRAME_STATE,
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
    }

    #[derive(Debug)]
    struct GeneratedFakeCode {
        id: u64,
        function_id: u32,
        tier: NativeFrameKind,
    }

    #[derive(Debug)]
    struct SplicedFakeCode {
        source: GeneratedFakeCode,
        spliced: Box<[u32]>,
        records: Box<[SafepointRecord]>,
    }

    impl JitFunctionCode for SplicedFakeCode {
        fn metadata(&self) -> CodeObjectMetadata {
            let mut metadata = self.source.metadata();
            metadata.safepoint_count = self.records.len() as u32;
            metadata.frame_map_count = self.records.len() as u32;
            metadata
        }

        fn native_frame_kind(&self) -> NativeFrameKind {
            self.source.native_frame_kind()
        }

        fn spliced_functions(&self) -> &[u32] {
            &self.spliced
        }

        fn code_len(&self) -> usize {
            self.source.code_len()
        }

        fn entry_addr(&self) -> Option<usize> {
            self.source.entry_addr()
        }

        fn safepoint_record(&self, id: SafepointId) -> Option<&SafepointRecord> {
            self.records.iter().find(|record| record.id == id)
        }
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

        fn code_len(&self) -> usize {
            4
        }

        fn entry_addr(&self) -> Option<usize> {
            Some(0x10_0000 + self.id as usize * 16)
        }
    }

    #[test]
    fn resolves_safepoints_across_distinct_code_objects() {
        let mut registry = JitCodeRegistry::new_boxed();
        assert!(
            registry
                .register(
                    7,
                    Arc::new(FakeCode {
                        id: 7,
                        records: vec![SafepointRecord::window(3, NO_FRAME_STATE)],
                        dependencies: Vec::new().into_boxed_slice(),
                    }),
                )
                .is_ok()
        );
        assert!(
            registry
                .register(
                    9,
                    Arc::new(FakeCode {
                        id: 9,
                        records: vec![SafepointRecord::window(5, NO_FRAME_STATE)],
                        dependencies: Vec::new().into_boxed_slice(),
                    }),
                )
                .is_ok()
        );

        let view = unsafe { *(registry.view_addr() as *const CodeRegistryView) };
        let hit = unsafe { view.resolve(9, 5) }.expect("nested callee record resolves");
        assert_eq!(unsafe { (*hit).id }, 5);
        assert!(unsafe { &*hit }.spill_roots.is_empty());
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
            records: vec![SafepointRecord::window(1, NO_FRAME_STATE)],
            dependencies: Vec::new().into_boxed_slice(),
        });
        let anchor = code.clone();
        assert!(registry.register(11, code.clone()).is_ok());
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
        assert!(
            registry
                .register_generation(13, code.clone(), 2, 9, Some(1), Box::new([]),)
                .is_ok()
        );
        let cell_addr = registry.entry_cell_addr(13).expect("entry cell installed");
        assert_eq!(
            registry.entry_cell_addr_for_entry(code.as_ref()),
            Some(cell_addr)
        );
        // SAFETY: entry cells are boxed and retained for the registry lifetime.
        let cell = unsafe { &*(cell_addr as *const CodeEntryCell) };
        assert_eq!(cell.code_object_id, 13);
        let function_cell = &registry.function_entry_cells[&cell.native_frame_header.function_id];
        assert_eq!(function_cell.param_count, 2);
        assert_eq!(function_cell.register_count, 9);
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
    fn generated_tiering_suppression_belongs_to_one_generation() {
        let mut registry = JitCodeRegistry::new_boxed();
        for (id, tier, expected) in [
            (301, NativeFrameKind::Baseline, 1),
            (302, NativeFrameKind::Baseline, 1),
            (303, NativeFrameKind::Optimizing, 0),
        ] {
            let code: Arc<dyn JitFunctionCode> = Arc::new(GeneratedFakeCode {
                id,
                function_id: 7,
                tier,
            });
            assert!(
                registry
                    .register_generation(id, code, 0, 4, Some(1), Box::new([]),)
                    .is_ok()
            );
            assert_eq!(
                registry
                    .published_function_entry(7)
                    .unwrap()
                    .1
                    .generated_tiering_enabled
                    .get(),
                expected
            );
            registry.suppress_generated_tiering(7);
            assert_eq!(
                registry
                    .published_function_entry(7)
                    .unwrap()
                    .1
                    .generated_tiering_enabled
                    .get(),
                0
            );
        }
        registry.suppress_generated_tiering(999);
    }

    #[test]
    fn linked_interpreter_entries_survive_directory_growth_and_invalidation() {
        let mut registry = JitCodeRegistry::new_boxed();
        registry.ensure_function_entry(0, 2, 9, 5, 0);
        let stable = registry.function_entries[0];
        let destination = registry.function_entry_cells[&0].current_generation();
        let (_, entry) = registry.published_function_entry(0).unwrap();
        assert_eq!(entry.native_frame_header.kind, NativeFrameKind::Interpreter);
        assert_eq!(entry.native_frame_header.register_count, 9);
        assert_eq!(entry.code_object_id, 0);
        assert_eq!(entry.flags, 0);
        assert_eq!(
            entry.entry_addr.load(std::sync::atomic::Ordering::Acquire),
            crate::native_abi::call_generic_entry as *const () as u64
        );
        for function_id in 1..4096 {
            registry.ensure_function_entry(function_id, 0, 1, 0, 0);
        }
        assert_eq!(registry.view.function_entry_count, 4096);
        assert_eq!(
            registry.view.function_entries,
            registry.function_entries.as_ptr() as u64
        );
        assert_eq!(registry.function_entries[0], stable);
        assert_eq!(
            registry.function_entry_cells[&0].current_generation(),
            destination
        );
        registry.invalidate_all();
        assert_eq!(
            registry.function_entry_cells[&0].current_generation(),
            destination
        );
        // SAFETY: every address is owned by a permanent registry box.
        let directory = unsafe {
            std::slice::from_raw_parts(registry.view.function_entries as *const u64, 4096)
        };
        assert!(directory.iter().all(|address| *address != 0));
    }

    #[test]
    fn interpreter_feedback_is_distinct_per_function_and_drains_once() {
        let mut registry = JitCodeRegistry::new_boxed();
        for (id, entries, deopts) in [(7, 3, 1), (8, 5, 2)] {
            registry.ensure_function_entry(id, 1, 4, 0, 0);
            let cell = registry.function_entry_cells[&id].interpreter_destination();
            cell.generated_entries.set(entries);
            cell.generated_deopts.set(deopts);
        }
        let feedback = registry.take_generated_feedback();
        let observed: Vec<_> = feedback
            .iter()
            .map(|delta| {
                assert_eq!(delta.code_object_id, 0);
                assert_eq!(delta.tier, NativeFrameKind::Interpreter);
                (
                    delta.function_id,
                    delta.entries,
                    delta.returns,
                    delta.deopts,
                )
            })
            .collect();
        assert_eq!(observed, [(7, 3, 2, 1), (8, 5, 3, 2)]);
        assert!(registry.take_generated_feedback().is_empty());
        registry.function_entry_cells[&7]
            .interpreter_destination()
            .generated_entries
            .set(4);
        let next = registry.take_generated_feedback();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].function_id, 7);
        assert_eq!(
            (next[0].entries, next[0].returns, next[0].deopts),
            (1, 1, 0)
        );
    }

    #[test]
    fn stable_function_entry_switches_tiers_without_invalidating_callers() {
        let mut registry = JitCodeRegistry::new_boxed();
        let baseline: Arc<dyn JitFunctionCode> = Arc::new(GeneratedFakeCode {
            id: 101,
            function_id: 7,
            tier: NativeFrameKind::Baseline,
        });
        let optimizing: Arc<dyn JitFunctionCode> = Arc::new(GeneratedFakeCode {
            id: 102,
            function_id: 7,
            tier: NativeFrameKind::Optimizing,
        });
        let caller: Arc<dyn JitFunctionCode> = Arc::new(GeneratedFakeCode {
            id: 201,
            function_id: 8,
            tier: NativeFrameKind::Baseline,
        });

        assert!(registry.published_function_entry(7).is_none());
        assert!(
            registry
                .register_generation(101, baseline, 2, 9, Some(1), Box::new([]),)
                .is_ok()
        );
        assert_eq!(
            registry
                .published_function_entry(7)
                .unwrap()
                .1
                .code_object_id,
            101
        );
        assert!(
            registry
                .register_generation(201, caller, 2, 12, Some(1), Box::new([]),)
                .is_ok()
        );
        let stable_addr = std::ptr::from_ref(registry.function_entry_cells[&7].as_ref()) as u64;
        assert_eq!(
            registry.function_entry_cells[&7].current_generation(),
            registry.entry_cell_addr(101).unwrap()
        );

        assert!(
            registry
                .register_generation(102, optimizing, 2, 9, Some(1), Box::new([]),)
                .is_ok()
        );
        assert_eq!(
            registry
                .published_function_entry(7)
                .unwrap()
                .1
                .code_object_id,
            102
        );
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
        // SAFETY: generation cells are stable for the registry lifetime.
        let optimizing_cell =
            unsafe { &*(registry.entry_cell_addr(102).unwrap() as *const CodeEntryCell) };
        assert_eq!(registry.function_entry_cells[&7].register_count, 9);
        assert_eq!(optimizing_cell.native_frame_header.register_count, 9);

        assert_eq!(registry.invalidate_code_object(102), vec![7]);
        assert_eq!(
            registry
                .published_function_entry(7)
                .unwrap()
                .1
                .code_object_id,
            101
        );
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
        });
        assert!(
            registry
                .register_generation(103, optimizing_refresh, 2, 9, Some(1), Box::new([]),)
                .is_ok()
        );
        assert_eq!(registry.invalidate_code_object(101), vec![7]);
        assert_eq!(
            registry.function_entry_cells[&7].current_generation(),
            registry.entry_cell_addr(103).unwrap(),
            "refreshing baseline must preserve the independent optimizing generation"
        );
        assert_eq!(
            registry
                .published_function_entry(7)
                .unwrap()
                .1
                .code_object_id,
            103
        );
        assert_eq!(registry.invalidate_function(7), vec![7]);
        registry.retire_unreferenced();
        let (function, destination) = registry.published_function_entry(7).unwrap();
        assert_eq!(std::ptr::from_ref(function) as u64, stable_addr);
        assert_eq!(destination.code_object_id, 0);
        assert_eq!(
            destination.native_frame_header.kind,
            NativeFrameKind::Interpreter
        );
        assert_ne!(
            destination
                .entry_addr
                .load(std::sync::atomic::Ordering::Acquire),
            0
        );
        assert!(
            registry.entry_cell_addr(103).is_some(),
            "retired cells remain owned tombstones"
        );
    }

    #[test]
    fn invalidating_source_unlinks_both_tiers_and_all_spliced_callers_only() {
        let mut registry = JitCodeRegistry::new_boxed();
        for (id, fid, tier, spliced) in [
            (301, 7, NativeFrameKind::Baseline, vec![]),
            (302, 7, NativeFrameKind::Optimizing, vec![]),
            (303, 10, NativeFrameKind::Optimizing, vec![7]),
            (304, 11, NativeFrameKind::Baseline, vec![7]),
            (305, 12, NativeFrameKind::Optimizing, vec![]),
        ] {
            let code: Arc<dyn JitFunctionCode> = Arc::new(SplicedFakeCode {
                source: GeneratedFakeCode {
                    id,
                    function_id: fid,
                    tier,
                },
                spliced: spliced.into_boxed_slice(),
                records: Box::default(),
            });
            assert!(
                registry
                    .register_generation(id, code, 1, 4, Some(1), Box::new([]),)
                    .is_ok()
            );
        }
        let stable_source = std::ptr::from_ref(registry.function_entry_cells[&7].as_ref()) as u64;
        let ordinary_destination = registry.function_entry_cells[&12].current_generation();
        assert_eq!(registry.invalidate_function(7), [7, 10, 11]);
        for id in 301..=304 {
            assert!(!registry.is_current_generation(id));
            assert_eq!(registry.codes[&id].state, CodeLifetimeState::Invalid);
            assert!(
                registry.codes[&id].code.safepoint_count() == 0,
                "splices without deopt or safepoint records still invalidate"
            );
        }
        for fid in [7, 10, 11] {
            let (_, destination) = registry.published_function_entry(fid).unwrap();
            assert_eq!(destination.code_object_id, 0);
            assert_eq!(
                destination.native_frame_header.kind,
                NativeFrameKind::Interpreter
            );
        }
        assert_eq!(
            std::ptr::from_ref(registry.function_entry_cells[&7].as_ref()) as u64,
            stable_source,
            "ordinary callers retain the permanent source entry address"
        );
        assert!(registry.is_current_generation(305));
        assert_eq!(
            registry.function_entry_cells[&12].current_generation(),
            ordinary_destination
        );
    }

    #[test]
    fn same_tier_replacement_cannot_resurrect_old_code_and_keeps_active_roots() {
        let mut registry = JitCodeRegistry::new_boxed();
        let baseline: Arc<dyn JitFunctionCode> = Arc::new(GeneratedFakeCode {
            id: 601,
            function_id: 7,
            tier: NativeFrameKind::Baseline,
        });
        assert!(
            registry
                .register_generation(601, baseline, 0, 1, Some(1), Box::new([]),)
                .is_ok()
        );
        let old: Arc<dyn JitFunctionCode> = Arc::new(SplicedFakeCode {
            source: GeneratedFakeCode {
                id: 602,
                function_id: 7,
                tier: NativeFrameKind::Optimizing,
            },
            spliced: Box::default(),
            records: Box::new([SafepointRecord::window(9, NO_FRAME_STATE)]),
        });
        assert!(
            registry
                .register_generation(602, old.clone(), 0, 1, Some(1), Box::new([]),)
                .is_ok()
        );
        let cell_addr = registry.entry_cell_addr(602).unwrap();
        // SAFETY: cells are boxed permanent registry records, even after unlink.
        let cell = unsafe { &*(cell_addr as *const CodeEntryCell) };
        let lease = cell.try_acquire().unwrap();
        let newest: Arc<dyn JitFunctionCode> = Arc::new(GeneratedFakeCode {
            id: 603,
            function_id: 7,
            tier: NativeFrameKind::Optimizing,
        });
        assert!(
            registry
                .register_generation(603, newest, 0, 1, Some(1), Box::new([]),)
                .is_ok()
        );
        assert_eq!(registry.codes[&602].state, CodeLifetimeState::Invalid);
        assert!(!registry.is_current_for_entry(old.as_ref()));
        assert_eq!(registry.codes[&603].state, CodeLifetimeState::Installed);
        assert_eq!(
            registry
                .published_function_entry(7)
                .unwrap()
                .1
                .code_object_id,
            603
        );
        assert!(
            cell.try_acquire().is_none(),
            "no new entry into superseded code"
        );
        drop(old);
        assert_eq!(
            registry.retire_unreferenced(),
            0,
            "active code retains exact roots"
        );
        assert!(registry.safepoint_record(602, 9).is_some());
        assert_eq!(registry.invalidate_code_object(603), [7]);
        assert_eq!(
            registry
                .published_function_entry(7)
                .unwrap()
                .1
                .code_object_id,
            601,
            "fallback selects the independent baseline, never the superseded optimizer"
        );
        assert!(registry.is_current_generation(601));
        drop(lease);
        assert_eq!(registry.retire_unreferenced(), 2);
        assert!(registry.safepoint_record(602, 9).is_none());
        assert_eq!(
            registry
                .published_function_entry(7)
                .unwrap()
                .1
                .code_object_id,
            601
        );
    }

    #[test]
    fn rejected_same_tier_replacement_preserves_previous_publication() {
        let mut registry = JitCodeRegistry::new_boxed();
        let account = otter_resource::ResourceAccount::new(
            otter_resource::ResourceLimits::builder()
                .limit(
                    otter_resource::ResourceClass::GeneratedCodeBytes,
                    4 + std::mem::size_of::<CodeEntryCell>() as u64,
                )
                .build(),
        );
        registry.set_account(account);
        let old: Arc<dyn JitFunctionCode> = Arc::new(GeneratedFakeCode {
            id: 701,
            function_id: 7,
            tier: NativeFrameKind::Optimizing,
        });
        assert!(
            registry
                .register_generation(701, old.clone(), 0, 1, Some(1), Box::new([]),)
                .is_ok()
        );
        let rejected: Arc<dyn JitFunctionCode> = Arc::new(GeneratedFakeCode {
            id: 702,
            function_id: 7,
            tier: NativeFrameKind::Optimizing,
        });
        assert!(
            !registry
                .register_generation(702, rejected, 0, 1, Some(1), Box::new([]),)
                .is_ok()
        );
        assert!(registry.is_current_for_entry(old.as_ref()));
        assert!(!registry.codes.contains_key(&702));
        assert!(!registry.entry_cells.contains_key(&702));
        assert_eq!(
            registry
                .published_function_entry(7)
                .unwrap()
                .1
                .code_object_id,
            701
        );
    }

    #[derive(Debug)]
    struct SizedCode {
        source: GeneratedFakeCode,
        retained: u64,
    }

    impl JitFunctionCode for SizedCode {
        fn metadata(&self) -> CodeObjectMetadata {
            self.source.metadata()
        }
        fn native_frame_kind(&self) -> NativeFrameKind {
            self.source.native_frame_kind()
        }
        fn code_len(&self) -> usize {
            self.source.code_len()
        }
        fn entry_addr(&self) -> Option<usize> {
            self.source.entry_addr()
        }
        fn retained_bytes(&self) -> u64 {
            self.retained
        }
    }

    #[test]
    fn source_work_admission_accounts_active_invalid_mappings_and_host_headroom() {
        let mut registry = JitCodeRegistry::new_boxed();
        let cap = crate::tier_policy::JIT_CODE_RESOURCE_LIMIT_BYTES;
        let entry_bytes = std::mem::size_of::<CodeEntryCell>() as u64;
        let active: Arc<dyn JitFunctionCode> = Arc::new(SizedCode {
            source: GeneratedFakeCode {
                id: 801,
                function_id: 7,
                tier: NativeFrameKind::Baseline,
            },
            retained: cap - entry_bytes - 4,
        });
        assert!(
            registry
                .register_generation(801, active.clone(), 0, 1, None, Box::new([]),)
                .is_ok()
        );
        assert_eq!(registry.available_code_bytes(), 4);
        registry.invalidate_code_object(801);
        assert_eq!(
            registry.available_code_bytes(),
            4,
            "invalid active mapping is still charged"
        );
        let replacement: Arc<dyn JitFunctionCode> = Arc::new(SizedCode {
            source: GeneratedFakeCode {
                id: 802,
                function_id: 7,
                tier: NativeFrameKind::Baseline,
            },
            retained: 8,
        });
        assert!(
            !registry
                .register_generation(802, replacement.clone(), 0, 1, None, Box::new([]),)
                .is_ok()
        );
        assert_eq!(
            registry.retire_unreferenced(),
            0,
            "active lease retains the mapping"
        );
        drop(active);
        assert_eq!(registry.retire_unreferenced(), 1);
        assert_eq!(
            registry.available_code_bytes(),
            cap - entry_bytes,
            "retired tombstone stays charged"
        );
        assert!(
            registry
                .register_generation(802, replacement, 0, 1, None, Box::new([]),)
                .is_ok()
        );
        assert_eq!(registry.available_code_bytes(), cap - 8 - 2 * entry_bytes);

        let mut limited = JitCodeRegistry::new_boxed();
        let account = otter_resource::ResourceAccount::new(
            otter_resource::ResourceLimits::builder()
                .limit(
                    otter_resource::ResourceClass::GeneratedCodeBytes,
                    12 + entry_bytes,
                )
                .build(),
        );
        limited.set_account(account.clone());
        assert_eq!(limited.available_code_bytes(), 12 + entry_bytes);
        let code: Arc<dyn JitFunctionCode> = Arc::new(GeneratedFakeCode {
            id: 803,
            function_id: 8,
            tier: NativeFrameKind::Baseline,
        });
        assert!(
            limited
                .register_generation(803, code.clone(), 0, 1, None, Box::new([]),)
                .is_ok()
        );
        limited.invalidate_code_object(803);
        assert_eq!(limited.available_code_bytes(), 8);
        assert_eq!(
            account
                .snapshot()
                .get(otter_resource::ResourceClass::GeneratedCodeBytes)
                .current(),
            4 + entry_bytes
        );
        drop(code);
        assert_eq!(limited.retire_unreferenced(), 1);
        assert_eq!(limited.available_code_bytes(), 12);
    }

    #[test]
    fn source_work_finalized_payload_refusal_reports_full_demand_and_rolls_back() {
        let mut registry = JitCodeRegistry::new_boxed();
        let dependency = CodeDependency::epoch(CodeDependencyKind::Protector, 1, 0);
        let code = fake_code(811, vec![dependency]);
        let roots = Box::new([crate::jit_roots::CompilationRoot::CalleeIdentity(Arc::new(
            crate::jit_roots::CalleeIdentityCell::new(),
        ))]);
        let required = JitCodeRegistry::retained_admission_bytes(code.as_ref(), &roots[..]);
        assert!(required > code.retained_bytes());
        let account = otter_resource::ResourceAccount::new(
            otter_resource::ResourceLimits::builder()
                .limit(
                    otter_resource::ResourceClass::GeneratedCodeBytes,
                    required - 1,
                )
                .build(),
        );
        registry.set_account(account.clone());
        let function = crate::executable::CodeBlock::jit_test_stub(0, 0, 1, &[], &[]);
        assert_eq!(
            registry.install_compiled(811, code, &function, None, roots),
            Err(JitInstallError::ResourceBudget {
                required_bytes: required
            })
        );
        assert!(registry.codes.is_empty());
        assert!(registry.entry_cells.is_empty());
        assert_eq!(
            account
                .snapshot()
                .get(otter_resource::ResourceClass::GeneratedCodeBytes)
                .current(),
            0,
            "mapping lease must roll back when persistent entry reservation fails"
        );
        let wrong_function = crate::executable::CodeBlock::jit_test_stub(7, 0, 1, &[], &[]);
        assert_eq!(
            registry.install_compiled(
                812,
                fake_code(812, vec![]),
                &wrong_function,
                None,
                Box::new([])
            ),
            Err(JitInstallError::InvalidCode)
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
    fn generated_code_bytes_are_charged_limited_and_released() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let code = fake_code(61, Vec::new());
        let per_object = code.retained_bytes();
        let account = otter_resource::ResourceAccount::new(
            otter_resource::ResourceLimits::builder()
                .limit(
                    otter_resource::ResourceClass::GeneratedCodeBytes,
                    per_object,
                )
                .build(),
        );
        interp
            .set_resource_account(account.clone())
            .expect("an idle interpreter installs against any budget");

        assert!(interp.jit_code_registry.register(61, code).is_ok());
        let entry = *account
            .snapshot()
            .get(otter_resource::ResourceClass::GeneratedCodeBytes);
        assert_eq!(entry.current(), per_object);

        // The budget is exhausted: the next install declines without side
        // effects and the rejection is visible on the ledger.
        assert!(
            !interp
                .jit_code_registry
                .register(62, fake_code(62, Vec::new()))
                .is_ok()
        );
        assert!(!interp.jit_code_registry.codes.contains_key(&62));
        let entry = *account
            .snapshot()
            .get(otter_resource::ResourceClass::GeneratedCodeBytes);
        assert_eq!(entry.current(), per_object);
        assert_eq!(entry.rejections(), 1);

        // Physical retirement releases the charge with the object.
        interp.jit_code_registry.invalidate_code_object(61);
        interp.jit_code_registry.retire_unreferenced();
        assert!(!interp.jit_code_registry.codes.contains_key(&61));
        let entry = *account
            .snapshot()
            .get(otter_resource::ResourceClass::GeneratedCodeBytes);
        assert_eq!(entry.current(), 0);

        // With the charge released the registry admits new code again.
        assert!(
            interp
                .jit_code_registry
                .register(63, fake_code(63, Vec::new()))
                .is_ok()
        );
    }

    #[test]
    fn protector_bump_invalidates_only_stale_matching_dependencies() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let protector = CodeDependency::epoch(
            CodeDependencyKind::Protector,
            ARRAY_INDEX_ACCESSOR_PROTECTOR_IDENTITY,
            0,
        );
        assert!(
            interp
                .jit_code_registry
                .register(21, fake_code(21, vec![protector]))
                .is_ok()
        );
        assert!(
            interp
                .jit_code_registry
                .register(22, fake_code(22, Vec::new()))
                .is_ok()
        );
        assert!(
            interp
                .jit_code_registry
                .register(
                    23,
                    fake_code(
                        23,
                        vec![CodeDependency::epoch(
                            CodeDependencyKind::Protector,
                            ARRAY_INDEX_ACCESSOR_PROTECTOR_IDENTITY + 1,
                            0,
                        )],
                    ),
                )
                .is_ok()
        );

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
                .is_ok()
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
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
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
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
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
                    .is_ok()
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
                .is_ok()
        );
    }
}
