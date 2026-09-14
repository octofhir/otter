//! Shared registry of linked code chunks.
//!
//! Every compiled [`BytecodeModule`] an interpreter executes — entry
//! scripts, module graphs, `eval` bodies, `new Function` bodies,
//! dynamic-import fragments — links into one interpreter-wide
//! function-id space. Linking rebases the module's function ids,
//! function-id constants, module-init records, and named-property IC
//! sites by the registry's running totals, so a function value
//! (closure, class constructor, plain function id) created in one
//! chunk stays resolvable after it escapes to a frame executing a
//! different chunk.
//!
//! This is the ownership shape production engines use: a JSC
//! `JSFunction` resolves through its own `Executable` rather than an
//! ambient per-script table, so code born in `eval` outlives the eval
//! turn. Otter keeps ids dense and chunk-relative instead of holding a
//! per-value code pointer, which leaves [`crate::Frame`],
//! [`crate::closure::JsClosure`], and [`crate::Value`] layouts
//! untouched. A foreign id is first resolved to its owning
//! [`crate::ExecutionContext`]; table access is always chunk-local.
//!
//! # Contents
//!
//! - [`CodeSpace`] — append-only chunk index with monotonic function-id and
//!   IC-site bases.
//! - [`ChunkPayload`] — one live linked chunk's tables and retained-byte lease.
//! - [`ResolvedCtx`] — borrowed-or-owned context for one function id.
//!
//! # Invariants
//!
//! - Node ranges are immutable after publication. Resolution binary-searches
//!   the published index under its shared lock and then locks only the
//!   matching payload slot; no resolution or census is proportional to the
//!   number of linked chunks. Linking holds one single-writer lock from
//!   global-base selection through publication.
//! - Chunks are appended with monotonically increasing `function_base` values,
//!   so index order is id order.
//! - Fresh compiler output is rebased fallibly and verified exactly once at
//!   its assigned base before publication. Decoded cache carriers retain the
//!   same proof through proof-preserving rebasing and executable building.
//! - A rejected module leaves the registry unchanged; admission constructs the
//!   immutable executable and atom tables only after every fallible check has
//!   succeeded.
//! - A linked module's `Function::id`, `Constant::FunctionId`, and
//!   `ModuleInit::function_id` are all rebased before the executable
//!   view is built, so chunk bytecode only ever materialises global
//!   ids at runtime.
//! - Registry entries hold no [`crate::ExecutionContext`] (and thus no
//!   registry handle), so linked chunks never form an `Arc` cycle.
//! - Eviction leaves an immutable tombstone node. Function ids and IC-site
//!   ranges are never reused, and an evicted id is distinguishable from an id
//!   that was never linked.
//! - Every live payload carries the exact `SourceModuleBytes` lease for its
//!   retained bytecode, executable view, and atom table. Dropping the payload
//!   drops the physical tables and the charge in the same operation.
//! - IC-site bases keep dense property-IC ids globally unique, so two
//!   chunks never alias one interpreter IC slot.
//!
//! # See also
//!
//! - [`crate::execution_context`]
//! - [`crate::executable`]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};

use otter_bytecode::{
    BytecodeModule, BytecodeRebaseError, BytecodeVerifyError, Constant, Op, VerifiedBytecodeModule,
};
use otter_resource::{ResourceAccount, ResourceClass, ResourceError, ResourceLease};

use crate::ExecutionContext;
use crate::executable::ExecutableModule;
use crate::property_atom::AtomTable;

/// One live chunk's tables and exact retained-byte ownership.
///
/// Execution contexts clone this one `Arc`, rather than cloning the three
/// table Arcs independently. Consequently `Arc::strong_count == 1` on the
/// registry slot is an exact proof that no context still retains the payload.
#[derive(Debug)]
pub(crate) struct ChunkPayload {
    pub(crate) module: Arc<BytecodeModule>,
    pub(crate) executable: Arc<ExecutableModule>,
    pub(crate) atoms: Arc<AtomTable>,
    retained_bytes: u64,
    /// Drops in the same payload as the retained tables.
    _retained_lease: ResourceLease,
}

impl ChunkPayload {
    #[must_use]
    pub(crate) fn retained_bytes(&self) -> u64 {
        self.retained_bytes
    }
}

/// Whether a linked chunk participates in automatic reclamation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChunkRetention {
    /// Entry/bootstrap chunks remain installed for the isolate lifetime.
    Pinned,
    /// Eval and on-demand module chunks may be reclaimed after liveness proof.
    Evictable,
}

/// Result of resolving a global function id through the immutable node chain.
#[derive(Debug)]
pub(crate) enum ChunkResolution {
    /// The id belongs to a live payload.
    Live {
        function_base: u32,
        payload: Arc<ChunkPayload>,
    },
    /// The id belongs to a linked range whose payload has been reclaimed.
    Evicted {
        function_base: u32,
        function_count: u32,
    },
    /// No linked node ever owned the id.
    Unlinked,
}

/// Immutable candidate metadata copied out before liveness and JIT retirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChunkEvictionCandidate {
    pub(crate) function_base: u32,
    pub(crate) function_count: u32,
    pub(crate) property_ic_site_base: u32,
    pub(crate) property_ic_site_end: u32,
    pub(crate) module_identity: usize,
    pub(crate) retained_bytes: u64,
}

impl ChunkEvictionCandidate {
    #[must_use]
    pub(crate) fn function_end(self) -> u32 {
        self.function_base + self.function_count
    }
}

/// Result of attempting to tombstone one previously selected node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChunkEvictionResult {
    /// The payload and its lease were dropped.
    Evicted { retained_bytes: u64 },
    /// A context acquired the payload after candidate selection.
    RetainedContext,
    /// The node is pinned, already evicted, or no longer matches the candidate.
    NotEligible,
}

/// Append-only node registry of every code chunk linked into one interpreter.
///
/// Published chunks live in one vector ordered by `function_base`, so a
/// function id resolves to its owning node by binary search. Readers hold the
/// shared lock only for that search; linking appends under the exclusive lock
/// as its final, infallible step, after every fallible admission check.
#[derive(Debug, Default)]
pub(crate) struct CodeSpace {
    chunks: RwLock<Vec<Arc<CodeChunk>>>,
    /// Single-writer link lock, held from global-base selection through
    /// publication so two links cannot select the same bases.
    link: Mutex<()>,
    /// Advances after every topology or payload-state publication. Turn-local
    /// owner caches compare this before reusing a context, so linking or
    /// eviction cannot leave a stale fast-path answer.
    epoch: AtomicU64,
    /// Exact bytes owned by live evictable payloads. Link and eviction update
    /// this in O(1); telemetry must not turn repeated `eval` linking into an
    /// O(n²) walk of the append-only chunk chain.
    evictable_retained_bytes: AtomicU64,
}

/// Typed failure to admit a bytecode module into a [`CodeSpace`].
///
/// Admission errors are deterministic and leave the registry untouched. The
/// wrapped verifier error identifies malformed bytecode; the remaining
/// variants describe code-space range failures that only become known after
/// the next global bases are selected.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BytecodeLinkError {
    /// Structural bytecode verification failed.
    Verify(BytecodeVerifyError),
    /// A retained verification proof could not be moved to the selected base.
    RebaseVerified(BytecodeRebaseError),
    /// Adding a code-space base to one function-id-bearing record overflowed.
    FunctionIdRebaseOverflow {
        /// Record family being rebased.
        record: &'static str,
        /// Original base-zero function id.
        function_id: u32,
        /// Code-space base assigned to the chunk.
        base: u32,
    },
    /// A chunk's dense function-id range does not fit in `u32`.
    FunctionIdCapacity {
        /// First requested function id.
        base: u32,
        /// Number of functions in the chunk.
        function_count: usize,
    },
    /// A chunk's dense property-IC site range does not fit in `u32`.
    PropertyIcCapacity {
        /// First requested property-IC site id.
        base: u32,
        /// Number of property-IC sites in the chunk.
        site_count: usize,
    },
    /// The chunk's retained bytes were rejected by the linking account's
    /// `SourceModuleBytes` budget.
    RetainedBytes(ResourceError),
}

impl std::fmt::Display for BytecodeLinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Verify(error) => write!(f, "invalid bytecode module: {error}"),
            Self::RebaseVerified(error) => {
                write!(f, "cannot rebase verified bytecode module: {error}")
            }
            Self::FunctionIdRebaseOverflow {
                record,
                function_id,
                base,
            } => write!(
                f,
                "rebasing {record} function id {function_id} by code-space base {base} exceeds u32"
            ),
            Self::FunctionIdCapacity {
                base,
                function_count,
            } => write!(
                f,
                "function range from code-space base {base} with {function_count} entries exceeds u32"
            ),
            Self::PropertyIcCapacity { base, site_count } => write!(
                f,
                "property-IC range from code-space base {base} with {site_count} sites exceeds u32"
            ),
            Self::RetainedBytes(error) => {
                write!(f, "cannot admit linked chunk's retained bytes: {error}")
            }
        }
    }
}

impl std::error::Error for BytecodeLinkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Verify(error) => Some(error),
            Self::RebaseVerified(error) => Some(error),
            Self::RetainedBytes(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ResourceError> for BytecodeLinkError {
    fn from(error: ResourceError) -> Self {
        Self::RetainedBytes(error)
    }
}

impl From<BytecodeVerifyError> for BytecodeLinkError {
    fn from(error: BytecodeVerifyError) -> Self {
        Self::Verify(error)
    }
}

impl From<BytecodeRebaseError> for BytecodeLinkError {
    fn from(error: BytecodeRebaseError) -> Self {
        Self::RebaseVerified(error)
    }
}

/// Function-id and IC-site bases the chunk after `node` would start at.
fn next_bases(node: &CodeChunk) -> Result<(u32, u32), BytecodeLinkError> {
    let function_base = node.function_base.checked_add(node.function_count).ok_or(
        BytecodeLinkError::FunctionIdCapacity {
            base: node.function_base,
            function_count: node.function_count as usize,
        },
    )?;
    Ok((function_base, node.property_ic_site_end))
}

/// One immutable-range registry node. Eviction clears only `payload` and
/// leaves this tombstone in the index.
#[derive(Debug)]
struct CodeChunk {
    function_base: u32,
    function_count: u32,
    property_ic_site_base: u32,
    property_ic_site_end: u32,
    retention: ChunkRetention,
    payload: RwLock<Option<Arc<ChunkPayload>>>,
}

/// Exact bytes one live payload retains.
fn chunk_retained_bytes(
    module: &BytecodeModule,
    executable: &ExecutableModule,
    atoms: &AtomTable,
) -> u64 {
    (std::mem::size_of::<ChunkPayload>() as u64)
        .saturating_add(std::mem::size_of::<BytecodeModule>() as u64)
        .saturating_add(module.retained_bytes())
        .saturating_add(std::mem::size_of::<ExecutableModule>() as u64)
        .saturating_add(executable.retained_bytes())
        .saturating_add(std::mem::size_of::<AtomTable>() as u64)
        .saturating_add(atoms.retained_bytes())
}

fn ensure_function_id_capacity(base: u32, function_count: usize) -> Result<u32, BytecodeLinkError> {
    let count =
        u32::try_from(function_count).map_err(|_| BytecodeLinkError::FunctionIdCapacity {
            base,
            function_count,
        })?;
    base.checked_add(count)
        .ok_or(BytecodeLinkError::FunctionIdCapacity {
            base,
            function_count,
        })?;
    Ok(count)
}

fn ensure_property_ic_capacity(
    module: &BytecodeModule,
    base: u32,
) -> Result<(), BytecodeLinkError> {
    let site_count = module
        .functions
        .iter()
        .map(|function| {
            function
                .code
                .iter()
                .filter(|instruction| {
                    matches!(
                        instruction.op,
                        Op::LoadProperty | Op::StoreProperty | Op::CallMethodValue
                    )
                })
                .count()
        })
        .try_fold(0usize, usize::checked_add)
        .ok_or(BytecodeLinkError::PropertyIcCapacity {
            base,
            site_count: usize::MAX,
        })?;
    let count = u32::try_from(site_count)
        .map_err(|_| BytecodeLinkError::PropertyIcCapacity { base, site_count })?;
    base.checked_add(count)
        .ok_or(BytecodeLinkError::PropertyIcCapacity { base, site_count })?;
    Ok(())
}

fn build_chunk(
    verified: VerifiedBytecodeModule,
    function_base: u32,
    function_count: u32,
    property_ic_base: u32,
    retention: ChunkRetention,
    account: &ResourceAccount,
) -> Result<Arc<CodeChunk>, BytecodeLinkError> {
    let executable = Arc::new(ExecutableModule::from_verified_bytecode_with_ic_base(
        &verified,
        property_ic_base,
    ));
    let module = verified.into_module();
    let atoms = Arc::new(AtomTable::from_constants(&module.constants));
    let retained_bytes = chunk_retained_bytes(&module, &executable, &atoms);
    // Retained-bytes admission: a rejected budget declines the link before
    // publication and the rejection is visible on the ledger.
    let retained_lease = account.reserve_exact(ResourceClass::SourceModuleBytes, retained_bytes)?;
    let payload = Arc::new(ChunkPayload {
        module: Arc::new(module),
        executable,
        atoms,
        retained_bytes,
        _retained_lease: retained_lease,
    });
    let property_ic_site_end = payload.executable.property_ic_site_end();
    Ok(Arc::new(CodeChunk {
        function_base,
        function_count,
        property_ic_site_base: property_ic_base,
        property_ic_site_end,
        retention,
        payload: RwLock::new(Some(payload)),
    }))
}

/// Append a fully built chunk to the index. Called with the link lock held;
/// the exclusive index lock is taken only for the push itself.
fn publish_chunk(space: &CodeSpace, chunk: Arc<CodeChunk>) {
    if chunk.retention == ChunkRetention::Evictable {
        let retained_bytes = chunk
            .payload
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .expect("published chunks start with a live payload")
            .retained_bytes();
        space
            .evictable_retained_bytes
            .fetch_add(retained_bytes, Ordering::AcqRel);
    }
    space
        .chunks
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(chunk);
    space.epoch.fetch_add(1, Ordering::Release);
}

impl CodeSpace {
    /// Current registry publication epoch for turn-local owner caches.
    #[inline]
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Rebase `module` onto this registry's id space and append it as a new
    /// immutable chunk. Returns the chunk's [`ExecutionContext`] bound to this
    /// code space.
    ///
    /// `Interpreter::link_module` is the normal single-writer entry point. The
    /// one-shot chain remains safe for concurrent callers because the append
    /// lock covers base selection through publication.
    ///
    /// # Errors
    /// Returns a typed verification, code-space capacity, or retained-bytes
    /// admission error. No chunk is published on failure.
    pub(crate) fn link_module(
        self: &Arc<Self>,
        module: BytecodeModule,
        account: &ResourceAccount,
    ) -> Result<ExecutionContext, BytecodeLinkError> {
        self.link_module_with_retention(module, account, ChunkRetention::Pinned)
    }

    /// Link a chunk that may be reclaimed after an explicit liveness proof.
    pub(crate) fn link_evictable_module(
        self: &Arc<Self>,
        module: BytecodeModule,
        account: &ResourceAccount,
    ) -> Result<ExecutionContext, BytecodeLinkError> {
        self.link_module_with_retention(module, account, ChunkRetention::Evictable)
    }

    fn link_module_with_retention(
        self: &Arc<Self>,
        mut module: BytecodeModule,
        account: &ResourceAccount,
        retention: ChunkRetention,
    ) -> Result<ExecutionContext, BytecodeLinkError> {
        let _link = self
            .link
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (function_base, property_ic_base) = self.next_bases()?;
        let function_count = ensure_function_id_capacity(function_base, module.functions.len())?;
        ensure_property_ic_capacity(&module, property_ic_base)?;

        rebase_module(&mut module, function_base)?;
        let verified = VerifiedBytecodeModule::new_at_base(module, function_base)?;

        let chunk = build_chunk(
            verified,
            function_base,
            function_count,
            property_ic_base,
            retention,
            account,
        )?;
        publish_chunk(self, Arc::clone(&chunk));
        let payload = chunk
            .payload
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .expect("newly built chunk has a live payload")
            .clone();
        Ok(ExecutionContext::from_chunk_payload(
            payload,
            function_base,
            Arc::clone(self),
        ))
    }

    /// Append a decoded/cache module while preserving its retained admission
    /// proof. Absolute function ids are translated to the selected code-space
    /// base without re-running wordcode or metadata verification.
    ///
    /// # Errors
    /// Returns a typed capacity, proof-rebase, retained-bytes admission, or
    /// publication error. No chunk is published on failure.
    pub(crate) fn link_verified_module(
        self: &Arc<Self>,
        verified: VerifiedBytecodeModule,
        account: &ResourceAccount,
    ) -> Result<ExecutionContext, BytecodeLinkError> {
        let _link = self
            .link
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (function_base, property_ic_base) = self.next_bases()?;
        let function_count =
            ensure_function_id_capacity(function_base, verified.module().functions.len())?;
        ensure_property_ic_capacity(verified.module(), property_ic_base)?;

        let verified = verified.rebase_to(function_base)?;
        let chunk = build_chunk(
            verified,
            function_base,
            function_count,
            property_ic_base,
            ChunkRetention::Pinned,
            account,
        )?;
        publish_chunk(self, Arc::clone(&chunk));
        let payload = chunk
            .payload
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .expect("newly built chunk has a live payload")
            .clone();
        Ok(ExecutionContext::from_chunk_payload(
            payload,
            function_base,
            Arc::clone(self),
        ))
    }

    /// Bases the next link must use, from the last published chunk.
    fn next_bases(&self) -> Result<(u32, u32), BytecodeLinkError> {
        match self.chunks().last() {
            Some(chunk) => next_bases(chunk),
            None => Ok((0, 0)),
        }
    }

    fn chunks(&self) -> RwLockReadGuard<'_, Vec<Arc<CodeChunk>>> {
        self.chunks
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The published node whose dense id range contains `function_id`.
    fn chunk_for(chunks: &[Arc<CodeChunk>], function_id: u32) -> Option<&Arc<CodeChunk>> {
        let after = chunks.partition_point(|chunk| chunk.function_base <= function_id);
        let chunk = chunks[..after].last()?;
        (function_id - chunk.function_base < chunk.function_count).then_some(chunk)
    }

    /// Resolve the node owning `function_id` as live, evicted, or never linked.
    pub(crate) fn resolve_chunk(&self, function_id: u32) -> ChunkResolution {
        let chunks = self.chunks();
        let Some(current) = Self::chunk_for(&chunks, function_id) else {
            return ChunkResolution::Unlinked;
        };
        let payload = current
            .payload
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match payload.as_ref() {
            Some(payload) => ChunkResolution::Live {
                function_base: current.function_base,
                payload: Arc::clone(payload),
            },
            None => ChunkResolution::Evicted {
                function_base: current.function_base,
                function_count: current.function_count,
            },
        }
    }

    /// Resolve every linked chunk's property-name atoms against `names`.
    ///
    /// Called when an interpreter adopts a code space it did not link: the
    /// chunks' atom tables may carry no ids at all (a standalone
    /// [`ExecutionContext::from_module`]) or ids from another isolate's
    /// interner, and either would compare wrongly against shapes keyed by the
    /// adopting interpreter's atoms. Resolution is idempotent, so the walk is
    /// safe to repeat.
    pub(crate) fn resolve_atoms(&self, names: &crate::property_atom::NameInterner) {
        for current in self.chunks().iter() {
            if let Some(payload) = current
                .payload
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                payload.atoms.resolve(names);
            }
        }
    }

    /// Read one function's material-feedback epoch without requiring an
    /// ambient execution context. Used only by the explicit optimizing-tier
    /// policy query; baseline compilation and dispatch do not call it.
    pub(crate) fn feedback_epoch(&self, function_id: u32) -> Option<u32> {
        let ChunkResolution::Live {
            function_base,
            payload,
        } = self.resolve_chunk(function_id)
        else {
            return None;
        };
        payload
            .executable
            .function(function_id - function_base)
            .map(crate::executable::CodeBlock::feedback_epoch)
    }

    pub(crate) fn trace_property_ic_roots(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        for current in self.chunks().iter() {
            if let Some(payload) = current
                .payload
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                payload.executable.trace_property_ic_roots(visitor);
            }
        }
    }

    pub(crate) fn property_ic_stats(&self) -> crate::property_ic::PropertyIcStats {
        let mut total = crate::property_ic::PropertyIcStats::default();
        for current in self.chunks().iter() {
            let payload = current
                .payload
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(payload) = payload.as_ref() else {
                continue;
            };
            let stats = payload.executable.property_ic_stats();
            total.load_hits = total.load_hits.saturating_add(stats.load_hits);
            total.load_misses = total.load_misses.saturating_add(stats.load_misses);
            total.load_installs = total.load_installs.saturating_add(stats.load_installs);
            total.load_disables = total.load_disables.saturating_add(stats.load_disables);
            total.store_hits = total.store_hits.saturating_add(stats.store_hits);
            total.store_misses = total.store_misses.saturating_add(stats.store_misses);
            total.store_installs = total.store_installs.saturating_add(stats.store_installs);
            total.store_disables = total.store_disables.saturating_add(stats.store_disables);
        }
        total
    }

    #[cfg(test)]
    pub(crate) fn polymorphic_property_count(
        &self,
        kind: crate::property_ic::PropertyIcKind,
    ) -> usize {
        self.chunks()
            .iter()
            .filter_map(|current| {
                current
                    .payload
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                    .map(|payload| payload.executable.polymorphic_property_count(kind))
            })
            .sum()
    }

    pub(crate) fn property_ic_snapshots(&self) -> Vec<crate::inspect::IcSiteSnapshot> {
        let mut out = Vec::new();
        for current in self.chunks().iter() {
            if let Some(payload) = current
                .payload
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                out.extend(payload.executable.property_ic_snapshots());
            }
        }
        out
    }

    /// Snapshot reclaimable payloads, largest first with base as a stable tie
    /// breaker. A payload with a retained context is omitted: its physical
    /// tables and lease cannot be released even if no function id is live.
    #[must_use]
    pub(crate) fn eviction_candidates(&self) -> Vec<ChunkEvictionCandidate> {
        let mut candidates = Vec::new();
        for current in self.chunks().iter() {
            if current.retention == ChunkRetention::Evictable {
                let payload = current
                    .payload
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(payload) = payload.as_ref()
                    && Arc::strong_count(payload) == 1
                {
                    candidates.push(ChunkEvictionCandidate {
                        function_base: current.function_base,
                        function_count: current.function_count,
                        property_ic_site_base: current.property_ic_site_base,
                        property_ic_site_end: current.property_ic_site_end,
                        module_identity: Arc::as_ptr(&payload.module) as usize,
                        retained_bytes: payload.retained_bytes(),
                    });
                }
            }
        }
        candidates.sort_unstable_by_key(|candidate| {
            (
                std::cmp::Reverse(candidate.retained_bytes),
                candidate.function_base,
            )
        });
        candidates
    }

    /// Total bytes retained by live automatically reclaimable payloads.
    #[must_use]
    pub(crate) fn evictable_retained_bytes(&self) -> u64 {
        self.evictable_retained_bytes.load(Ordering::Acquire)
    }

    /// Clear one selected payload after the caller has proved its id range
    /// unreachable and physically retired generated code for that range.
    pub(crate) fn evict_candidate(&self, candidate: ChunkEvictionCandidate) -> ChunkEvictionResult {
        let chunks = self.chunks();
        let Some(current) = Self::chunk_for(&chunks, candidate.function_base) else {
            return ChunkEvictionResult::NotEligible;
        };
        if current.function_base != candidate.function_base
            || current.function_count != candidate.function_count
            || current.retention != ChunkRetention::Evictable
        {
            return ChunkEvictionResult::NotEligible;
        }
        let mut payload = current
            .payload
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(live) = payload.as_ref() else {
            return ChunkEvictionResult::NotEligible;
        };
        if Arc::strong_count(live) != 1 {
            return ChunkEvictionResult::RetainedContext;
        }
        let retained_bytes = live.retained_bytes();
        let dropped = payload.take();
        self.evictable_retained_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_sub(retained_bytes)
            })
            .expect("live evictable byte accounting covers its payload");
        self.epoch.fetch_add(1, Ordering::Release);
        drop(payload);
        drop(dropped);
        ChunkEvictionResult::Evicted { retained_bytes }
    }
}

/// A chunk resolved for one function id: either the caller's ambient
/// context (borrowed, the hot in-chunk path) or a context rebuilt from
/// a foreign registry chunk (owned, a few `Arc` clones).
#[derive(Debug)]
pub(crate) enum ResolvedCtx<'a> {
    Ambient(&'a ExecutionContext),
    Owned(ExecutionContext),
}

impl std::ops::Deref for ResolvedCtx<'_> {
    type Target = ExecutionContext;

    fn deref(&self) -> &ExecutionContext {
        match self {
            Self::Ambient(context) => context,
            Self::Owned(context) => context,
        }
    }
}

/// Shift every function-id-bearing record in `module` by `base` so the
/// chunk's ids are unique within the owning [`CodeSpace`].
fn rebase_module(module: &mut BytecodeModule, base: u32) -> Result<(), BytecodeLinkError> {
    if base == 0 {
        return Ok(());
    }
    for function in &mut module.functions {
        function.id =
            function
                .id
                .checked_add(base)
                .ok_or(BytecodeLinkError::FunctionIdRebaseOverflow {
                    record: "function",
                    function_id: function.id,
                    base,
                })?;
        for site in &mut function.class_hint_sites {
            site.class_function_id = site.class_function_id.checked_add(base).ok_or(
                BytecodeLinkError::FunctionIdRebaseOverflow {
                    record: "class hint",
                    function_id: site.class_function_id,
                    base,
                },
            )?;
        }
    }
    for constant in &mut module.constants {
        if let Constant::FunctionId { index } = constant {
            *index =
                index
                    .checked_add(base)
                    .ok_or(BytecodeLinkError::FunctionIdRebaseOverflow {
                        record: "constant",
                        function_id: *index,
                        base,
                    })?;
        }
    }
    for init in &mut module.module_inits {
        init.function_id = init.function_id.checked_add(base).ok_or(
            BytecodeLinkError::FunctionIdRebaseOverflow {
                record: "module init",
                function_id: init.function_id,
                base,
            },
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use otter_bytecode::{
        BytecodeModule, BytecodeVerifyError, Constant, Function, Instruction, ModuleInit, Op,
        Operand, SourceKind, VerifiedBytecodeModule,
    };

    use super::{BytecodeLinkError, CodeSpace, rebase_module};

    fn unlimited() -> otter_resource::ResourceAccount {
        otter_resource::ResourceAccount::default()
    }

    fn module_with_functions(count: u32) -> BytecodeModule {
        let functions = (0..count)
            .map(|id| Function {
                id,
                name: format!("f{id}"),
                // Tests overwrite `code` with hand-written bodies; give them a
                // window wide enough that the build-time register verifier
                // accepts any small register number they use.
                locals: 16,
                code: vec![Instruction {
                    pc: 0,
                    op: Op::ReturnUndefined,
                    operands: Vec::new(),
                }]
                .into(),
                ..Function::default()
            })
            .collect();
        BytecodeModule {
            module: "<test>".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions,
            constants: vec![Constant::FunctionId { index: 1 }],
            module_resolutions: Vec::new(),
            module_inits: vec![ModuleInit {
                url: "test:mod".to_string(),
                function_id: 1,
            }],
            function_source: None,
        }
    }

    #[test]
    fn first_chunk_links_at_base_zero_unrebased() {
        let space = Arc::new(CodeSpace::default());
        let context = space
            .link_module(module_with_functions(3), &unlimited())
            .expect("valid first chunk links");
        assert_eq!(context.function_base(), 0);
        assert_eq!(context.function_id_constant(0), Some(1));
        assert!(context.exec_function(0).is_some());
        assert!(context.exec_function(2).is_some());
        assert!(context.exec_function(3).is_none());
    }

    #[test]
    fn second_chunk_rebases_ids_constants_and_inits() {
        let space = Arc::new(CodeSpace::default());
        let _first = space
            .link_module(module_with_functions(3), &unlimited())
            .expect("valid first chunk links");
        let second = space
            .link_module(module_with_functions(2), &unlimited())
            .expect("valid second chunk links");
        assert_eq!(second.function_base(), 3);
        assert_eq!(second.function_id_constant(0), Some(4));
        assert_eq!(second.module_init_function_id("test:mod"), Some(4));
        assert!(second.exec_function(3).is_some());
        assert!(second.exec_function(4).is_some());
        assert!(
            second.exec_function(2).is_none(),
            "table access is local to the owning chunk",
        );
        let first_owner = second
            .for_function(2)
            .expect("sibling id resolves to an owned context");
        assert!(first_owner.exec_function(2).is_some());
        assert!(second.exec_function(5).is_none());
    }

    #[test]
    fn verified_cache_carrier_rebases_without_aliasing_existing_ids() {
        let space = Arc::new(CodeSpace::default());
        space
            .link_module(module_with_functions(3), &unlimited())
            .expect("first chunk links");
        let verified = VerifiedBytecodeModule::new(module_with_functions(2))
            .expect("cache fixture verifies once");

        let second = space
            .link_verified_module(verified, &unlimited())
            .expect("retained proof rebases onto the selected range");
        assert_eq!(second.function_base(), 3);
        assert_eq!(second.function(3).map(|function| function.id), Some(3));
        assert_eq!(second.function(4).map(|function| function.id), Some(4));
        assert_eq!(second.module_init_function_id("test:mod"), Some(4));
    }

    #[test]
    fn foreign_ids_resolve_through_any_linked_context() {
        let space = Arc::new(CodeSpace::default());
        let first = space
            .link_module(module_with_functions(3), &unlimited())
            .expect("valid first chunk links");
        let second = space
            .link_module(module_with_functions(2), &unlimited())
            .expect("valid second chunk links");
        let foreign = first.for_function(4).expect("second chunk's id resolves");
        assert_eq!(foreign.function_base(), 3);
        assert!(foreign.exec_function(4).is_some());
        assert_eq!(
            foreign.function(4).map(|f| f.name.as_str()),
            Some("f1"),
            "global id 4 is the second chunk's local function 1",
        );
        let back = second.for_function(0).expect("first chunk's id resolves");
        assert_eq!(back.function_base(), 0);
        assert!(first.for_function(5).is_err());
    }

    #[test]
    fn ic_sites_continue_across_chunks() {
        let space = Arc::new(CodeSpace::default());
        let mut module = module_with_functions(1);
        module.functions[0].code = vec![
            Instruction {
                pc: 0,
                op: Op::LoadProperty,
                operands: vec![
                    Operand::Register(0),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                ],
            },
            Instruction {
                pc: 1,
                op: Op::ReturnUndefined,
                operands: Vec::new(),
            },
        ]
        .into();
        module.constants = vec![Constant::String {
            utf16: "x".encode_utf16().collect(),
        }];
        module.module_inits.clear();
        let second_module = module.clone();
        let first = space
            .link_module(module, &unlimited())
            .expect("valid first chunk links");
        let second = space
            .link_module(second_module, &unlimited())
            .expect("valid second chunk links");
        assert_eq!(first.property_ic_site_end(), 1);
        assert_eq!(second.property_ic_site_end(), 2);
        assert_eq!(first.property_ic_site(0, 0), Some(0));
        assert_eq!(second.property_ic_site(1, 0), Some(1));
    }

    #[test]
    fn evicting_chunk_drops_its_owned_property_program_and_snapshot() {
        let space = Arc::new(CodeSpace::default());
        let mut module = module_with_functions(1);
        module.functions[0].code = vec![
            Instruction {
                pc: 0,
                op: Op::LoadProperty,
                operands: vec![
                    Operand::Register(0),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                ],
            },
            Instruction {
                pc: 1,
                op: Op::ReturnUndefined,
                operands: Vec::new(),
            },
        ]
        .into();
        module.constants = vec![Constant::String {
            utf16: "x".encode_utf16().collect(),
        }];
        module.module_inits.clear();

        let context = space
            .link_evictable_module(module, &unlimited())
            .expect("evictable property chunk links");
        context
            .property_feedback_slot(0, 0, crate::property_ic::PropertyIcKind::Load)
            .expect("CodeBlock property slot")
            .install(crate::cache_ir::CacheStub::default());
        assert_eq!(space.property_ic_snapshots().len(), 1);
        drop(context);

        let candidate = space
            .eviction_candidates()
            .into_iter()
            .next()
            .expect("unretained chunk is evictable");
        assert!(matches!(
            space.evict_candidate(candidate),
            super::ChunkEvictionResult::Evicted { .. }
        ));
        assert!(space.property_ic_snapshots().is_empty());
    }

    #[test]
    fn concurrent_linkers_claim_disjoint_function_ranges() {
        let space = Arc::new(CodeSpace::default());
        let mut joins = Vec::new();
        for _ in 0..4 {
            let space = Arc::clone(&space);
            joins.push(std::thread::spawn(move || {
                space
                    .link_module(module_with_functions(2), &unlimited())
                    .expect("valid concurrent chunk links")
                    .function_base()
            }));
        }

        let mut bases: Vec<_> = joins
            .into_iter()
            .map(|join| join.join().expect("code-space linker completes"))
            .collect();
        bases.sort_unstable();
        assert_eq!(bases, [0, 2, 4, 6]);
        for function_id in 0..8 {
            assert!(matches!(
                space.resolve_chunk(function_id),
                super::ChunkResolution::Live { .. }
            ));
        }
    }

    #[test]
    fn rejected_first_module_does_not_consume_base_zero() {
        let space = Arc::new(CodeSpace::default());
        let mut malformed = module_with_functions(2);
        malformed.functions[0].id = 7;

        assert!(matches!(
            space.link_module(malformed, &unlimited()),
            Err(BytecodeLinkError::Verify(BytecodeVerifyError::FunctionId {
                function_index: 0,
                expected: 0,
                actual: 7,
            }))
        ));
        assert!(space.chunks().is_empty());

        let context = space
            .link_module(module_with_functions(2), &unlimited())
            .expect("valid module still claims base zero");
        assert_eq!(context.function_base(), 0);
    }

    #[test]
    fn hostile_register_module_returns_error_without_panicking_or_publishing() {
        let space = Arc::new(CodeSpace::default());
        let mut malformed = module_with_functions(1);
        malformed.constants.clear();
        malformed.module_inits.clear();
        malformed.functions[0].code = vec![
            Instruction {
                pc: 0,
                op: Op::LoadUndefined,
                operands: vec![Operand::Register(16)],
            },
            Instruction {
                pc: 1,
                op: Op::ReturnUndefined,
                operands: Vec::new(),
            },
        ]
        .into();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            space.link_module(malformed, &unlimited())
        }));
        assert!(matches!(
            result,
            Ok(Err(BytecodeLinkError::Verify(
                BytecodeVerifyError::RegisterOperand {
                    function_index: 0,
                    instruction_pc: 0,
                    register: 16,
                    register_count: 16,
                    ..
                }
            )))
        ));
        assert!(space.chunks().is_empty());

        let context = space
            .link_module(module_with_functions(2), &unlimited())
            .expect("valid module still claims base zero");
        assert_eq!(context.function_base(), 0);
    }

    #[test]
    fn corrupt_cache_blobs_cannot_create_carriers_or_publish_code() {
        let space = Arc::new(CodeSpace::default());
        let mut malformed = module_with_functions(2);
        malformed.functions[0].code = vec![
            Instruction {
                pc: 0,
                op: Op::LoadUndefined,
                operands: vec![Operand::Register(16)],
            },
            Instruction {
                pc: 1,
                op: Op::ReturnUndefined,
                operands: Vec::new(),
            },
        ]
        .into();
        let bytes = otter_bytecode::binary::encode_module(&malformed);

        let decoded = std::panic::catch_unwind(|| otter_bytecode::binary::decode_module(&bytes));
        assert!(matches!(
            decoded,
            Ok(Err(otter_bytecode::binary::ModuleDecodeError::Verify(
                BytecodeVerifyError::RegisterOperand { .. }
            )))
        ));
        assert!(space.chunks().is_empty());

        let context = space
            .link_module(module_with_functions(2), &unlimited())
            .expect("failed cache admission leaves base zero available");
        assert_eq!(context.function_base(), 0);
    }

    #[test]
    fn hostile_closure_spine_is_rejected_before_jit_visible_code_exists() {
        let space = Arc::new(CodeSpace::default());
        let mut malformed = module_with_functions(2);
        malformed.functions[1].inherited_upvalue_count = 1;
        malformed.functions[0].code = vec![
            Instruction {
                pc: 0,
                op: Op::MakeClosure,
                operands: vec![
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(0),
                ],
            },
            Instruction {
                pc: 1,
                op: Op::ReturnUndefined,
                operands: Vec::new(),
            },
        ]
        .into();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            space.link_module(malformed, &unlimited())
        }));
        assert!(matches!(
            result,
            Ok(Err(BytecodeLinkError::Verify(
                BytecodeVerifyError::ClosureCaptureCount {
                    target_function_id: 1,
                    expected: 1,
                    actual: 0,
                    ..
                }
            )))
        ));
        assert!(space.chunks().is_empty());
    }

    #[test]
    fn linked_chunk_bytes_are_charged_limited_and_released() {
        use otter_resource::{ResourceAccount, ResourceClass, ResourceLimits};

        let space = Arc::new(CodeSpace::default());
        let account = unlimited();
        space
            .link_module(module_with_functions(2), &account)
            .expect("valid chunk links against an unlimited account");
        let per_chunk = account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .current();
        assert!(per_chunk > 0, "a linked chunk retains a nonzero charge");

        // A budget below one chunk declines the link with a typed error and
        // leaves the registry untouched; the rejection is on the ledger.
        let limited = ResourceAccount::new(
            ResourceLimits::builder()
                .limit(ResourceClass::SourceModuleBytes, per_chunk / 2)
                .build(),
        );
        let rejecting_space = Arc::new(CodeSpace::default());
        let error = rejecting_space
            .link_module(module_with_functions(2), &limited)
            .expect_err("budget below one chunk rejects the link");
        assert!(matches!(error, BytecodeLinkError::RetainedBytes(_)));
        assert!(rejecting_space.chunks().is_empty());
        let entry = *limited.snapshot().get(ResourceClass::SourceModuleBytes);
        assert_eq!(entry.current(), 0);
        assert_eq!(entry.rejections(), 1);

        // The failed admission leaves base zero claimable.
        rejecting_space
            .link_module(module_with_functions(2), &unlimited())
            .expect("valid module still claims base zero");

        // Dropping the registry releases every chunk's charge.
        drop(space);
        assert_eq!(
            account
                .snapshot()
                .get(ResourceClass::SourceModuleBytes)
                .current(),
            0
        );
    }

    #[test]
    fn evictable_payload_becomes_a_typed_tombstone_and_releases_its_lease() {
        use otter_resource::ResourceClass;

        let space = Arc::new(CodeSpace::default());
        let account = unlimited();
        let pinned = space
            .link_module(module_with_functions(2), &account)
            .expect("pinned chunk");
        let baseline = account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .current();
        let evictable = space
            .link_evictable_module(module_with_functions(2), &account)
            .expect("evictable chunk");
        let old_base = evictable.function_base();
        assert_eq!(old_base, 2);
        assert!(space.evictable_retained_bytes() > 0);
        assert!(
            account
                .snapshot()
                .get(ResourceClass::SourceModuleBytes)
                .current()
                > baseline
        );

        assert!(space.eviction_candidates().is_empty());
        drop(evictable);
        let candidates = space.eviction_candidates();
        let [candidate] = candidates.as_slice() else {
            panic!("one unretained dynamic chunk is eligible");
        };
        let candidate = *candidate;
        let retained_bytes = candidate.retained_bytes;
        assert_eq!(space.evictable_retained_bytes(), retained_bytes);
        let rescued = pinned
            .for_function(old_base)
            .expect("a context acquired after selection retains the payload");
        assert_eq!(
            space.evict_candidate(candidate),
            super::ChunkEvictionResult::RetainedContext,
        );
        drop(rescued);
        assert_eq!(
            space.evict_candidate(candidate),
            super::ChunkEvictionResult::Evicted { retained_bytes }
        );
        assert_eq!(space.evictable_retained_bytes(), 0);
        assert_eq!(
            account
                .snapshot()
                .get(ResourceClass::SourceModuleBytes)
                .current(),
            baseline
        );
        assert!(matches!(
            space.resolve_chunk(old_base),
            super::ChunkResolution::Evicted {
                function_base: 2,
                function_count: 2,
            }
        ));
        assert!(matches!(
            pinned.for_function(old_base),
            Err(crate::execution_context::FunctionResolutionError::Evicted {
                function_id: 2,
                function_base: 2,
                function_count: 2,
            })
        ));

        let later = space
            .link_module(module_with_functions(2), &account)
            .expect("later chunk");
        assert_eq!(later.function_base(), 4, "tombstoned ids are never reused");
    }

    #[test]
    fn rebase_overflow_is_typed_instead_of_panicking() {
        let mut module = module_with_functions(2);
        let error = rebase_module(&mut module, u32::MAX)
            .expect_err("second dense function id cannot be rebased");
        assert_eq!(
            error,
            BytecodeLinkError::FunctionIdRebaseOverflow {
                record: "function",
                function_id: 1,
                base: u32::MAX,
            }
        );
    }

    #[test]
    fn immutable_published_nodes_keep_code_space_and_context_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<CodeSpace>();
        assert_send_sync::<crate::ExecutionContext>();
    }
}
