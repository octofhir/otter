//! The isolate tables a compilation reads, shared with compiler threads.
//!
//! # Contents
//! - [`CompileShared`] — every table baking reads that the mutator also
//!   changes, each a [`crate::mutator_owned::MutatorOwned`] value, behind the
//!   one `Arc` the interpreter and its compiler threads hold.
//!
//! # Invariants
//! - The interpreter reaches these tables through its `MutatorToken`; baking
//!   reaches them only through `MutatorOwned::try_read` and goes without a
//!   fact whose table is being written.
//! - Values these tables hold that name GC cells are traced by the isolate's
//!   root walk on the mutator; no compiler thread reads while it rewrites
//!   them.
//!
//! # See also
//! - [`crate::mutator_owned`] — the ownership protocol.

use crate::mutator_owned::{MutatorOwned, MutatorWrite};
use crate::{
    Interpreter, Value, constructor_fast_path, handles, interp, jit, native_abi, object, regexp,
};

/// The isolate tables a compilation reads.
#[derive(Default)]
pub(crate) struct CompileShared {
    /// Weak family-identity index; never a root.
    pub(crate) constructor_families: crate::constructor_layout::ConstructorFamilies,
    /// Per-context literal cells for `LoadString` and `LoadBigInt`, keyed by
    /// linked chunk identity and constant-pool index. Both primitives are
    /// immutable with value semantics, so each literal materializes once, on
    /// its first execution or in compile preparation; compiled code reads
    /// the shared address-stable box. The isolate's root walk traces the
    /// values, so a moving collection rewrites them in place without
    /// invalidating relocations.
    pub(crate) literal_cells: MutatorOwned<rustc_hash::FxHashMap<(usize, u32), Box<Value>>>,
    /// Regex literal sites resolved once, keyed like the literal cells:
    /// the program cell (old, rooted here) and the flags every evaluation of
    /// the site births a young RegExp from.
    pub(crate) regexp_literals:
        MutatorOwned<rustc_hash::FxHashMap<(usize, u32), regexp::RegExpLiteralSite>>,
    /// Shared copy-on-write element slabs of array-literal boilerplate
    /// constants, keyed like the literal cells. Each slab and every
    /// string it holds is old and never written, so arrays of every site that
    /// names the constant share it until their first write, and compiled code
    /// may bake its address.
    pub(crate) array_boilerplates: MutatorOwned<
        rustc_hash::FxHashMap<(usize, u32), crate::array::elements::ElementSlabHandle>,
    >,
    /// Final hidden class of each `Op::NewObjectLiteral` site, keyed by the
    /// owning function id and the site's first key constant.
    pub(crate) object_literal_layouts:
        MutatorOwned<rustc_hash::FxHashMap<(u32, u32), handles::ObjectLayout>>,
    /// Lifetime optimizing exit evidence keyed by the owning source function,
    /// PC and reason, including sites spliced into different native callers.
    /// The same record owns the requested policy, count and IC population.
    pub(crate) jit_optimized_exit_profiles: MutatorOwned<
        std::collections::BTreeMap<(u32, u32, native_abi::ExitReason), jit::JitExitProfile>,
    >,
    /// Per-function parameter widening learned from failed optimized entry
    /// guards; see [`jit::JitCompileSnapshot::parameter_widening`].
    pub(crate) jit_parameter_widening:
        MutatorOwned<rustc_hash::FxHashMap<u32, Box<[jit::JitParameterWidening]>>>,
    /// Per-function cache for conservative base-class constructor initializers
    /// recognized by `constructor_fast_path`. Entries hold owned strings and
    /// source descriptors only; GC shape handles stay in `shape_runtime`.
    pub(crate) simple_constructor_init_cache: MutatorOwned<
        rustc_hash::FxHashMap<u32, Option<constructor_fast_path::SimpleConstructorInit>>,
    >,
    /// Final hidden class reached by a cached simple constructor initializer.
    /// These handles are traced explicitly because a moving GC must rewrite the
    /// cache slot, not only the owning `shape_runtime` transition table.
    pub(crate) simple_constructor_shape_cache:
        MutatorOwned<rustc_hash::FxHashMap<(u32, object::ShapeId), object::ShapeHandle>>,
    /// Intrinsic-prototype programs of each property atom with the layout
    /// they were built against.
    pub(crate) jit_intrinsic_programs: MutatorOwned<
        rustc_hash::FxHashMap<
            u32,
            (
                interp::jit_intrinsic_properties::IntrinsicLayoutStamp,
                Vec<jit::JitCacheIrProgram>,
            ),
        >,
    >,
    /// Isolate-owned method-only IC and target distributions. Executable
    /// property programs live in their CodeBlock feedback slots instead.
    pub(crate) method_feedback: MutatorOwned<interp::MethodFeedbackDirectory>,
    /// §9.1.1.4 GlobalEnvironmentRecord declarative record — script
    /// top-level `let` / `const` / `class` bindings, shared across
    /// every script and eval chunk in the realm and *not* reflected
    /// as global object properties. Cells start as the TDZ hole.
    pub(crate) global_lexicals:
        MutatorOwned<rustc_hash::FxHashMap<Box<str>, (crate::UpvalueCell, /* is_const */ bool)>>,
    /// Monotonic identity of the active realm's global declarative record's
    /// name set. Generated reads of object-record globals guard this live
    /// cell because a later script/eval lexical declaration may shadow an
    /// otherwise shape-stable global-object property.
    pub(crate) global_lexical_epoch: std::sync::atomic::AtomicU64,
}

// SAFETY: the mutator is every table's sole writer (`MutatorOwned`). The GC
// handles the tables hold are offsets a compiler thread follows only while it
// holds the isolate's heap-read right, which excludes collection; the values
// it reads may be stale and every fact it bakes from them is re-verified on
// the isolate thread before code depending on it is published.
unsafe impl Send for CompileShared {}
// SAFETY: as above.
unsafe impl Sync for CompileShared {}

/// The mutator's read (`name`) and write (`name_mut`) of each shared table.
macro_rules! shared_tables {
    ($($name:ident, $name_mut:ident: $ty:ty;)*) => {
        impl Interpreter {
            $(
                #[inline]
                pub(crate) fn $name(&self) -> &$ty {
                    self.compile_shared.$name.get(&self.mutator)
                }

                #[inline]
                pub(crate) fn $name_mut(&mut self) -> MutatorWrite<'_, $ty> {
                    self.compile_shared.$name.get_mut(&mut self.mutator)
                }
            )*
        }
    };
}

shared_tables! {
    literal_cells, literal_cells_mut: rustc_hash::FxHashMap<(usize, u32), Box<Value>>;
    regexp_literals, regexp_literals_mut: rustc_hash::FxHashMap<(usize, u32), regexp::RegExpLiteralSite>;
    array_boilerplates, array_boilerplates_mut: rustc_hash::FxHashMap<(usize, u32), crate::array::elements::ElementSlabHandle>;
    object_literal_layouts, object_literal_layouts_mut: rustc_hash::FxHashMap<(u32, u32), handles::ObjectLayout>;
    jit_optimized_exit_profiles, jit_optimized_exit_profiles_mut: std::collections::BTreeMap<(u32, u32, native_abi::ExitReason), jit::JitExitProfile>;
    jit_parameter_widening, jit_parameter_widening_mut: rustc_hash::FxHashMap<u32, Box<[jit::JitParameterWidening]>>;
    simple_constructor_init_cache, simple_constructor_init_cache_mut: rustc_hash::FxHashMap<u32, Option<constructor_fast_path::SimpleConstructorInit>>;
    simple_constructor_shape_cache, simple_constructor_shape_cache_mut: rustc_hash::FxHashMap<(u32, object::ShapeId), object::ShapeHandle>;
    jit_intrinsic_programs, jit_intrinsic_programs_mut: rustc_hash::FxHashMap<u32, (interp::jit_intrinsic_properties::IntrinsicLayoutStamp, Vec<jit::JitCacheIrProgram>)>;
    method_feedback, method_feedback_mut: interp::MethodFeedbackDirectory;
    global_lexicals, global_lexicals_mut: rustc_hash::FxHashMap<Box<str>, (crate::UpvalueCell, bool)>;
}

impl Interpreter {
    /// The global declarative record's name-set identity.
    #[inline]
    pub(crate) fn global_lexical_epoch(&self) -> u64 {
        self.compile_shared
            .global_lexical_epoch
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A lexical declaration changed the global declarative record's names.
    pub(crate) fn bump_global_lexical_epoch(&mut self) {
        let epoch = &self.compile_shared.global_lexical_epoch;
        epoch.store(
            epoch
                .load(std::sync::atomic::Ordering::Relaxed)
                .wrapping_add(1),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Install `epoch` as the active realm's and return the one it replaces.
    pub(crate) fn swap_global_lexical_epoch(&mut self, epoch: u64) -> u64 {
        self.compile_shared
            .global_lexical_epoch
            .swap(epoch, std::sync::atomic::Ordering::Relaxed)
    }
}
