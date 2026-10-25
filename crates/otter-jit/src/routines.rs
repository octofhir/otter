//! Machine-code routines every code object of one compiler calls instead of
//! carrying its own copy, and the code space that owns them.
//!
//! The truthiness routine decides `ToBoolean` of the values optimized code
//! does not test inline. The named-property routines select a site's IC slot handler, probe the
//! isolate's shared action table and, on a miss, enter the committed runtime
//! operation. Each is emitted once per isolate, when a site first calls it,
//! into the compiler's [`CodeMemory`]; baseline sites call them with their
//! fixed register inputs. Optimized megamorphic loads call a probe entry that
//! preserves every allocatable register but its result and completes no miss;
//! optimized megamorphic stores call through the committed runtime ABI.
//!
//! # Contents
//! - [`NativeCodeSpace`] — one compiler's executable memory and routines.
//! - [`Routine`] — which routine a call site enters.
//! - [`Routines`] — the routines of one isolate, each emitted on first use.
//! - [`RoutineRequest`] — one compile's access to its isolate's routines.
//!
//! # Invariants
//! - A routine embeds only isolate-wide facts: object, shape and string
//!   layouts, the cage base, the shared action table and runtime stub entries.
//!   Routine sets are keyed by the isolate-specific ones, so every code object
//!   of an isolate calls the same routines.
//! - A routine is placed before any code that calls it is emitted, so the
//!   barrier that publishes a caller to its executing thread publishes the
//!   routine as well.
//! - Every code object that calls a routine retains its set, so routines
//!   outlive every caller.
//!
//! # See also
//! - `crate::template::arm64::shared_property` and its x86-64 counterpart
//!   emit the routines.
//! - [`crate::code_memory`] places them.

use std::sync::{Arc, Mutex, OnceLock};

use otter_vm::JitCompileSnapshot;

use crate::code_memory::CodeMemory;
use crate::{CompiledCode, Unsupported};

/// A routine generated code calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Routine {
    /// Named load over a baseline site's IC slot, with the committed miss.
    Load,
    /// Named store over a baseline site's IC slot, with the committed miss.
    Store,
    /// Method selection without a miss call.
    Method,
    /// The nursery string-concatenation fit.
    Concat,
    /// Name-keyed table load of an element site.
    KeyedLoad,
    /// Name-keyed table store of an element site.
    KeyedStore,
    /// [`Self::Load`]'s selection alone, preserving every allocatable
    /// register but its result and reporting a miss instead of completing it.
    ProbeLoad,
    /// [`Self::Store`] entered through the committed runtime ABI.
    CommittedStore,
    /// `ToBoolean` of a value that is not an immediate optimized code tests
    /// inline, preserving every allocatable register.
    Truthy,
    /// The field-summary observation and barriers that follow a baseline
    /// site's inline own-field store.
    StoreTail,
}

impl Routine {
    /// Stable name for artifacts.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Load => "load",
            Self::Store => "store",
            Self::Method => "method",
            Self::Concat => "concat",
            Self::KeyedLoad => "keyedLoad",
            Self::KeyedStore => "keyedStore",
            Self::ProbeLoad => "probeLoad",
            Self::CommittedStore => "committedStore",
            Self::Truthy => "truthy",
            Self::StoreTail => "storeTail",
        }
    }

    /// The code object holding this routine, with which it is emitted.
    const fn object(self) -> RoutineObject {
        match self {
            Self::Load | Self::ProbeLoad => RoutineObject::Load,
            Self::Store | Self::CommittedStore => RoutineObject::Store,
            Self::Method => RoutineObject::Method,
            Self::Concat => RoutineObject::Concat,
            Self::KeyedLoad => RoutineObject::KeyedLoad,
            Self::KeyedStore => RoutineObject::KeyedStore,
            Self::Truthy => RoutineObject::Truthy,
            Self::StoreTail => RoutineObject::StoreTail,
        }
    }

    /// Whether this is the second entry of its object.
    const fn second(self) -> bool {
        matches!(self, Self::ProbeLoad | Self::CommittedStore)
    }
}

/// One emitted code object: a routine, and for load its probe entry and for
/// store its committed-ABI entry as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoutineObject {
    Load,
    Store,
    Method,
    Concat,
    KeyedLoad,
    KeyedStore,
    Truthy,
    StoreTail,
}

impl RoutineObject {
    const COUNT: usize = 8;
}

/// Entry offsets of one routine object: the routine and its second entry,
/// which is the routine itself for an object that has none or whose
/// committed-ABI inputs already match.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RoutineOffsets {
    pub(crate) entry: usize,
    pub(crate) second: usize,
}

/// One placed routine object.
#[derive(Debug)]
struct RoutineCode {
    _code: CompiledCode,
    entry: u64,
    second: u64,
}

/// The routines of one isolate, each emitted on first use.
#[derive(Debug)]
pub(crate) struct Routines {
    objects: [OnceLock<Result<RoutineCode, Unsupported>>; RoutineObject::COUNT],
}

impl Routines {
    fn entry(
        &self,
        routine: Routine,
        view: &JitCompileSnapshot,
        transitions: &crate::entry::TransitionTable,
        memory: &Arc<CodeMemory>,
    ) -> Result<u64, Unsupported> {
        let object = routine.object();
        let code = self.objects[object as usize]
            .get_or_init(|| place(object, view, transitions, memory))
            .as_ref()
            .map_err(Clone::clone)?;
        Ok(if routine.second() {
            code.second
        } else {
            code.entry
        })
    }
}

/// The isolate-specific facts a routine embeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RoutineKey {
    cage_base: usize,
    action_cache: Option<otter_vm::jit::JitPropertyActionCache>,
}

impl RoutineKey {
    fn of(view: &JitCompileSnapshot) -> Self {
        Self {
            cage_base: view.cage_base,
            action_cache: view.property_action_cache,
        }
    }
}

/// One compiler's executable memory and the routines its code calls.
#[derive(Debug)]
pub struct NativeCodeSpace {
    memory: Arc<CodeMemory>,
    routines: Mutex<Vec<(RoutineKey, Arc<Routines>)>>,
}

impl NativeCodeSpace {
    /// An empty code space.
    #[must_use]
    pub fn new() -> Self {
        Self {
            memory: CodeMemory::new(),
            routines: Mutex::new(Vec::new()),
        }
    }

    /// The memory every code object of this space is placed in.
    pub(crate) fn memory(&self) -> &Arc<CodeMemory> {
        &self.memory
    }

    /// The routines of `view`'s isolate.
    fn routines(&self, view: &JitCompileSnapshot) -> Arc<Routines> {
        let key = RoutineKey::of(view);
        let mut sets = self
            .routines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, set)) = sets.iter().find(|(known, _)| *known == key) {
            return Arc::clone(set);
        }
        let set = Arc::new(Routines {
            objects: std::array::from_fn(|_| OnceLock::new()),
        });
        sets.push((key, Arc::clone(&set)));
        set
    }
}

impl Default for NativeCodeSpace {
    fn default() -> Self {
        Self::new()
    }
}

/// One compile's access to its isolate's routines. The set is looked up when
/// the first site calls a routine, and each routine is emitted when any
/// compile first calls it; a failure to emit one is reported once the
/// compile finishes.
pub(crate) struct RoutineRequest<'a> {
    space: &'a NativeCodeSpace,
    view: &'a JitCompileSnapshot,
    transitions: &'a crate::entry::TransitionTable,
    set: std::cell::OnceCell<Arc<Routines>>,
    failure: std::cell::Cell<Option<Unsupported>>,
}

impl<'a> RoutineRequest<'a> {
    pub(crate) fn new(
        space: &'a NativeCodeSpace,
        view: &'a JitCompileSnapshot,
        transitions: &'a crate::entry::TransitionTable,
    ) -> Self {
        Self {
            space,
            view,
            transitions,
            set: std::cell::OnceCell::new(),
            failure: std::cell::Cell::new(None),
        }
    }

    /// Entry address of `routine`; zero, with the failure kept for
    /// [`Self::finish`], when it could not be emitted.
    pub(crate) fn entry(&self, routine: Routine) -> u64 {
        let set = self.set.get_or_init(|| self.space.routines(self.view));
        set.entry(routine, self.view, self.transitions, &self.space.memory)
            .unwrap_or_else(|failure| {
                self.failure.set(Some(failure));
                0
            })
    }

    /// The routines the compile's code calls, to retain with it, or the
    /// failure to emit one of them.
    pub(crate) fn finish(self) -> Result<Option<Arc<Routines>>, Unsupported> {
        match self.failure.into_inner() {
            Some(failure) => Err(failure),
            None => Ok(self.set.into_inner()),
        }
    }
}

/// A routine request for an emitter fixture whose code may run: its space
/// is never released, so the routines outlive the fixture's code.
#[cfg(test)]
pub(crate) fn fixture_routines<'a>(
    view: &'a JitCompileSnapshot,
    transitions: &'a crate::entry::TransitionTable,
) -> RoutineRequest<'a> {
    RoutineRequest::new(
        Box::leak(Box::new(NativeCodeSpace::new())),
        view,
        transitions,
    )
}

fn place(
    object: RoutineObject,
    view: &JitCompileSnapshot,
    transitions: &crate::entry::TransitionTable,
    memory: &Arc<CodeMemory>,
) -> Result<RoutineCode, Unsupported> {
    let (buffer, offsets) = emit(object, view, transitions)?;
    let code = CompiledCode::new(memory, buffer, dynasmrt::AssemblyOffset(offsets.entry))?;
    // SAFETY: both offsets are assembler offsets inside `code`.
    let (entry, second) = unsafe { (code.entry_ptr(), code.ptr_at(offsets.second)) };
    Ok(RoutineCode {
        entry: entry as u64,
        second: second as u64,
        _code: code,
    })
}

#[cfg(target_arch = "aarch64")]
fn emit(
    object: RoutineObject,
    view: &JitCompileSnapshot,
    transitions: &crate::entry::TransitionTable,
) -> Result<(dynasmrt::ExecutableBuffer, RoutineOffsets), Unsupported> {
    let mut ops = dynasmrt::aarch64::Assembler::new()
        .map_err(|_| Unsupported::Backend(crate::BackendFailure::AssemblerAllocation))?;
    let mut relocations =
        crate::artifact::relocation::RelocationCapture::new(false).with_literal_pool();
    let offsets = match object {
        RoutineObject::Truthy => {
            crate::graph::arm64::truthy::emit_routine(&mut ops, &mut relocations, view)
        }
        _ => crate::template::arm64::shared_property::emit_routine(
            &mut ops,
            &mut relocations,
            transitions,
            view,
            object,
        ),
    };
    relocations.emit_literal_pool(&mut ops);
    Ok((crate::entry::finalize_assembler(ops)?, offsets))
}

#[cfg(target_arch = "x86_64")]
fn emit(
    object: RoutineObject,
    view: &JitCompileSnapshot,
    transitions: &crate::entry::TransitionTable,
) -> Result<(dynasmrt::ExecutableBuffer, RoutineOffsets), Unsupported> {
    let mut ops = dynasmrt::x64::Assembler::new()
        .map_err(|_| Unsupported::Backend(crate::BackendFailure::AssemblerAllocation))?;
    let mut relocations = crate::artifact::relocation::RelocationCapture::new(false);
    let offsets = match object {
        RoutineObject::Truthy => {
            crate::graph::x86_64::truthy::emit_routine(&mut ops, &mut relocations, view)
        }
        _ => crate::template::x86_64::shared_property::emit_routine(
            &mut ops,
            &mut relocations,
            transitions,
            view,
            object,
        ),
    };
    Ok((crate::entry::finalize_assembler(ops)?, offsets))
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
fn emit(
    _object: RoutineObject,
    _view: &JitCompileSnapshot,
    _transitions: &crate::entry::TransitionTable,
) -> Result<(dynasmrt::ExecutableBuffer, RoutineOffsets), Unsupported> {
    Err(Unsupported::OperandShape(
        "no routine backend on this target",
    ))
}
