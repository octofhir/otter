//! What each graph node reads and writes, as abstract heaps: the alias model
//! code motion and redundancy elimination decide on.
//!
//! An abstract heap names a set of memory locations. The heaps form a tree:
//! a heap overlaps another when one contains the other, and siblings never
//! overlap. Two own-field heaps overlap only when they name the same field
//! location; two context-slot heaps only at the same offset.
//!
//! # Contents
//! - [`AbstractHeap`] — the heap tree and [`AbstractHeap::overlaps`].
//! - [`Effects`] / [`clobberize`] — one node's reads and writes, and whether
//!   it dereferences an input whose type a guard before it proves.
//! - [`HeapSet`] — the union of the writes of a region, such as a loop body.
//!
//! # Invariants
//! - A node that may run JavaScript, call out, or enter the runtime on a
//!   slow path that can do either writes [`AbstractHeap::World`].
//! - A node that may collect writes [`AbstractHeap::Movement`]: a collection
//!   can move a slab, so a raw element base read before it is stale after.
//!   Tagged values stay valid; the collector rewrites them in place.
//! - A node whose result depends on memory reads every heap that holds that
//!   memory; immutable state (type tags, a closure's context and function,
//!   a context's parent, literal and identity cells) is read by no heap.
//! - `derefs` marks a node that addresses memory through an input whose
//!   type only an earlier guard or branch proves: such a node must not run
//!   where that proof does not hold.
//!
//! # See also
//! - [`super::licm`] — moves invariant nodes out of loops with this model.
//! - [`super::ir::Kind::properties`] — the register and deopt view of a node.

use smallvec::SmallVec;

use super::ir::Kind;

/// A set of memory locations, in a tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AbstractHeap {
    /// Everything: the root.
    World,
    /// JavaScript-visible heap state.
    Heap,
    /// Every own named field of every ordinary object.
    Fields,
    /// The own data fields of one property name (its atom).
    Field(u32),
    /// The map word of every cell: object shapes, and the object state a
    /// shape fixes (prototype role, extensibility, slot layout).
    MapWords,
    /// The logical `length` of every Array.
    ArrayLengths,
    /// Dense element storage geometry: count, capacity, kind, base, and the
    /// guards over them.
    ElementStorage,
    /// Element values.
    Elements,
    /// Every word of every context and closure-shaped record read through a
    /// raw byte offset.
    ContextSlots,
    /// The words at one byte offset.
    ContextSlot(i32),
    /// Exotic body slots of non-ordinary receivers (a RegExp's `lastIndex`).
    BodySlots,
    /// The named-property bag of every Array.
    ArrayExpandos,
    /// The own-property bag of every function and the lookup facts that
    /// route a named load to it.
    FunctionBags,
    /// The activation: its `this`, window registers and actual arguments.
    Frame,
    /// The dependency flag of the generation and the facts it subscribes to.
    Dependencies,
    /// The watched field types of shapes: an own-field store may widen one.
    FieldTypes,
    /// Structural facts: prototype chains, holder slots and the stability
    /// of shapes. Only an appending store or the runtime changes them; an
    /// in-graph store never writes a prototype-role object.
    ChainFacts,
    /// Watched context slots, which a context write may change.
    ContextFacts,
    /// Where movable storage lives; a collection rewrites it.
    Movement,
}

impl AbstractHeap {
    /// The heap containing this one; `None` for [`Self::World`].
    #[must_use]
    pub(crate) fn parent(self) -> Option<Self> {
        Some(match self {
            Self::World => return None,
            Self::Heap | Self::Frame | Self::Dependencies | Self::Movement => Self::World,
            Self::FieldTypes | Self::ChainFacts | Self::ContextFacts => Self::Dependencies,
            Self::Field(_) => Self::Fields,
            Self::ContextSlot(_) => Self::ContextSlots,
            Self::Fields
            | Self::MapWords
            | Self::ArrayLengths
            | Self::ElementStorage
            | Self::Elements
            | Self::ContextSlots
            | Self::BodySlots
            | Self::ArrayExpandos
            | Self::FunctionBags => Self::Heap,
        })
    }

    /// Whether `self` contains `other` (or is it).
    #[must_use]
    pub(crate) fn contains(self, other: Self) -> bool {
        let mut current = Some(other);
        while let Some(heap) = current {
            if heap == self {
                return true;
            }
            current = heap.parent();
        }
        false
    }

    /// Whether some location is in both heaps.
    #[must_use]
    pub(crate) fn overlaps(self, other: Self) -> bool {
        self.contains(other) || other.contains(self)
    }
}

/// One node's memory effects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Effects {
    pub(crate) reads: SmallVec<[AbstractHeap; 2]>,
    pub(crate) writes: SmallVec<[AbstractHeap; 2]>,
    /// The node addresses memory through an input whose type an earlier
    /// guard or branch proves.
    pub(crate) derefs: bool,
    /// The node's success proves a fact about its inputs (a type, shape,
    /// layout, range or identity) that later nodes may address memory by.
    pub(crate) guard: bool,
}

impl Effects {
    fn reads(heaps: &[AbstractHeap]) -> Self {
        Self {
            reads: heaps.iter().copied().collect(),
            ..Self::default()
        }
    }

    fn deref(heaps: &[AbstractHeap]) -> Self {
        Self {
            derefs: true,
            ..Self::reads(heaps)
        }
    }

    fn writes(reads: &[AbstractHeap], writes: &[AbstractHeap]) -> Self {
        Self {
            reads: reads.iter().copied().collect(),
            writes: writes.iter().copied().collect(),
            derefs: true,
            guard: false,
        }
    }

    fn guard(mut self) -> Self {
        self.guard = true;
        self
    }

    fn world() -> Self {
        Self::writes(&[AbstractHeap::World], &[AbstractHeap::World])
    }

    /// Whether any read overlaps a heap in `writes`.
    #[must_use]
    pub(crate) fn reads_overlap(&self, writes: &HeapSet) -> bool {
        self.reads.iter().any(|&read| writes.overlaps(read))
    }
}

/// A union of heaps.
#[derive(Debug, Clone, Default)]
pub(crate) struct HeapSet {
    heaps: SmallVec<[AbstractHeap; 8]>,
}

impl HeapSet {
    /// Add `heap`, dropping members it contains.
    pub(crate) fn add(&mut self, heap: AbstractHeap) {
        if self.heaps.iter().any(|&member| member.contains(heap)) {
            return;
        }
        self.heaps.retain(|member| !heap.contains(*member));
        self.heaps.push(heap);
    }

    /// Whether some member overlaps `heap`.
    #[must_use]
    pub(crate) fn overlaps(&self, heap: AbstractHeap) -> bool {
        self.heaps.iter().any(|&member| member.overlaps(heap))
    }

    /// Whether the set holds the whole world.
    #[must_use]
    pub(crate) fn is_world(&self) -> bool {
        self.heaps.contains(&AbstractHeap::World)
    }
}

/// The memory effects of `kind`.
#[must_use]
pub(crate) fn clobberize(kind: &Kind) -> Effects {
    let effects = heap_effects(kind);
    if proves_inputs(kind) {
        effects.guard()
    } else {
        effects
    }
}

/// Whether the success of `kind` proves a fact about its inputs.
fn proves_inputs(kind: &Kind) -> bool {
    match kind {
        Kind::CheckShapes { .. }
        | Kind::CheckElements { .. }
        | Kind::CheckElementPresent
        | Kind::CheckHoleyElementPresent(_)
        | Kind::CheckBounds
        | Kind::CheckNumber
        | Kind::CheckNotNullish { .. }
        | Kind::CheckNotHole
        | Kind::CheckedTaggedToInt32
        | Kind::CheckedTaggedToFloat64
        | Kind::CheckedFloat64ToInt32
        | Kind::CheckedTaggedToIndex
        | Kind::CheckedFloat64ToIndex
        | Kind::CheckFunction { .. }
        | Kind::CheckCellValue(_)
        | Kind::CheckNative(_)
        | Kind::CheckFunctionPrototypeCall(_)
        | Kind::CheckClosureContext(_) => true,
        Kind::LoadGuardedMethod {
            receiver_proved, ..
        } => !receiver_proved,
        Kind::LoadArrayLength { array_proved } => !array_proved,
        Kind::LoadTypedArrayLength => true,
        Kind::LoadArrayExpando(_) | Kind::LoadFunctionBag | Kind::StringCharCodeAt => true,
        _ => false,
    }
}

fn heap_effects(kind: &Kind) -> Effects {
    use AbstractHeap as H;
    match kind {
        // A receiver plan's proof reads a constructor's family head and its
        // prototype's validity cell, which any heap write may change.
        Kind::CheckReceiverPlan(_) => Effects::reads(&[H::Heap]),
        // Values, conversions and checks of values alone.
        Kind::ConstTagged(_)
        | Kind::ConstInt32(_)
        | Kind::ConstFloat64(_)
        | Kind::VirtualObject { .. }
        | Kind::Phi
        | Kind::LoadNewTarget
        | Kind::LoadClosure
        | Kind::LoadLiteral(_)
        | Kind::LoadCellValue(_)
        | Kind::AllocationProjection(_)
        | Kind::PrimitiveCompare(_)
        | Kind::Int32Add
        | Kind::Int32Sub
        | Kind::Int32AddWrapping
        | Kind::Int32SubWrapping
        | Kind::Int32Mul
        | Kind::Int32MulExact
        | Kind::Int32MulIdentifyZeros
        | Kind::Int32Div
        | Kind::Int32Mod
        | Kind::Int32Negate
        | Kind::Int32BitAnd
        | Kind::Int32BitOr
        | Kind::Int32BitXor
        | Kind::Int32BitNot
        | Kind::Int32ShiftLeft
        | Kind::Int32ShiftRight
        | Kind::Int32ShiftRightLogical
        | Kind::Uint32ShiftRightToFloat64
        | Kind::Int32Compare(_)
        | Kind::Float64Add
        | Kind::Float64Sub
        | Kind::Float64Mul
        | Kind::Float64Div
        | Kind::Float64Negate
        | Kind::Float64Mod
        | Kind::Float64Compare(_)
        | Kind::CheckedTaggedToInt32
        | Kind::CheckedTaggedToFloat64
        | Kind::Int32ToTagged
        | Kind::Float64ToTagged
        | Kind::Int32ToFloat64
        | Kind::TruncateFloat64ToInt32
        | Kind::CheckedFloat64ToInt32
        | Kind::CheckedTaggedToIndex
        | Kind::CheckedFloat64ToIndex
        | Kind::StrictEqual { .. }
        | Kind::NullishEqual { .. }
        | Kind::ToBoolean
        | Kind::LogicalNot
        | Kind::TestTypeOf { .. }
        | Kind::CheckNumber
        | Kind::CheckNotNullish { .. }
        | Kind::CheckBounds
        | Kind::CheckNative(_)
        | Kind::CheckFunction { .. }
        | Kind::CheckCellValue(_)
        | Kind::CheckClosureContext(_)
        | Kind::CheckNotHole
        | Kind::NativeLeaf(_)
        | Kind::BooleanToInt32
        // String content never changes; the node tests its own receiver.
        | Kind::StringCharCodeAt => Effects::default(),
        // A cell's map word decides the result. An unproved input is tested
        // for a cell first; a proved one is read directly.
        Kind::CheckShapes { cell_proved, .. } => {
            if *cell_proved {
                Effects::deref(&[H::MapWords])
            } else {
                Effects::reads(&[H::MapWords])
            }
        }
        Kind::LoadReceiverShape => Effects::reads(&[H::MapWords]),
        // A closure's context and a context's parent never change. Context
        // and closure words are read through values that are contexts or
        // closures by construction, never by a guard's proof.
        Kind::LoadContextParent | Kind::LoadClosureContext => Effects::default(),
        Kind::CheckFunctionPrototypeCall(_) => Effects::reads(&[H::Fields, H::MapWords]),
        Kind::CheckElements { .. } => Effects::reads(&[H::ElementStorage, H::MapWords]),
        Kind::CheckElementPresent | Kind::CheckHoleyElementPresent(_) => {
            Effects::deref(&[H::Elements])
        }
        Kind::LoadOwnField { atom, .. } => Effects::deref(&[H::Field(*atom)]),
        Kind::LoadTaggedField(offset) => Effects::reads(&[H::ContextSlot(*offset)]),
        Kind::LoadArrayLength { array_proved } => {
            if *array_proved {
                Effects::deref(&[H::ArrayLengths])
            } else {
                Effects::reads(&[H::ArrayLengths])
            }
        }
        Kind::LoadElementsLength { .. } => Effects::deref(&[H::ElementStorage]),
        // A view's count is fixed while its storage is; a named property
        // only appears through an appending store, which writes the map
        // words, or the runtime.
        Kind::LoadTypedArrayLength => Effects::reads(&[H::ElementStorage, H::MapWords]),
        Kind::LoadElementsBase { off_heap, .. } => {
            if *off_heap {
                Effects::deref(&[H::ElementStorage])
            } else {
                Effects::deref(&[H::ElementStorage, H::Movement])
            }
        }
        Kind::LoadElement(_)
        | Kind::LoadElementUint32ToFloat64
        | Kind::LoadHoleyFloat64Element(_) => Effects::deref(&[H::Elements]),
        Kind::LoadArrayExpando(_) => Effects::reads(&[H::ArrayExpandos]),
        // The active realm changes only across a call.
        Kind::LoadFunctionBag => Effects::reads(&[H::FunctionBags]),
        Kind::LoadNamedProperty { .. } => Effects::reads(&[H::Heap, H::Dependencies]),
        Kind::LoadGuardedMethod {
            receiver_proved,
            cell_read,
            ..
        } => {
            // A method a code-retained cell holds is fixed while the chain
            // facts hold; otherwise the holder's slot is read.
            let heaps: &[H] = match (*cell_read, *receiver_proved) {
                (true, true) => &[H::ChainFacts],
                (true, false) => &[H::MapWords, H::ChainFacts],
                (false, _) => &[H::Heap, H::ChainFacts],
            };
            if *receiver_proved {
                Effects::deref(heaps)
            } else {
                Effects::reads(heaps)
            }
        }
        Kind::LoadGlobalBinding(_) => Effects::reads(&[H::Fields, H::MapWords]),
        // The active realm changes only across a call.
        Kind::LoadGlobalThis => Effects::reads(&[H::Frame]),
        Kind::LoadThis
        | Kind::InitialRegister(_)
        | Kind::LoadWindow(_)
        | Kind::LoadArgumentCount
        | Kind::LoadActualArgument
        | Kind::SelectArgument { .. }
        | Kind::CheckArgumentsElided => Effects::reads(&[H::Frame]),
        Kind::CheckFieldTypes(_) => Effects::reads(&[H::FieldTypes]),
        Kind::CheckContextSlot { .. } => Effects::reads(&[H::ContextFacts]),
        Kind::CheckStableShapes(_) => Effects::reads(&[H::ChainFacts]),
        Kind::CheckDependencies => Effects::reads(&[H::Dependencies]),
        // Stores. A store into an existing field may change a watched field
        // type; an appending store also publishes a child shape and ends the
        // stability of the shape it leaves.
        Kind::StoreOwnField { atom, .. } => {
            Effects::writes(&[], &[H::Field(*atom), H::FieldTypes])
        }
        Kind::StoreNamedProperty { .. } => Effects::writes(
            &[H::MapWords],
            &[H::Fields, H::MapWords, H::FieldTypes, H::ChainFacts],
        ),
        Kind::StoreTaggedField(offset) => Effects::writes(&[], &[H::ContextSlot(*offset)]),
        Kind::StoreBodySlot(_) => Effects::writes(&[H::MapWords], &[H::BodySlots]),
        Kind::StoreElement(_) => Effects::writes(&[], &[H::Elements]),
        Kind::NoteContextWrite(_) => Effects::writes(&[], &[H::ContextFacts]),
        // Remembered-set bookkeeping no program reads.
        Kind::WriteBarrier | Kind::ElementWriteBarrier => Effects::default(),
        // Allocation: a fresh cell no earlier load could have read, and a
        // possible collection.
        Kind::AllocationGroup(_)
        | Kind::NativeNewContext(_)
        | Kind::CopyContext
        | Kind::NewClosure
        | Kind::NewArrayWithLength
        | Kind::NewReceiver(_) => Effects::writes(&[H::Dependencies], &[H::Movement]),
        Kind::PrimitiveAdd | Kind::BigIntBinary(_) => Effects::writes(&[], &[H::Movement]),
        // Slow paths that may run JavaScript, and calls.
        Kind::NewObject
        | Kind::NewArrayEmpty
        | Kind::NewObjectLiteral { .. }
        | Kind::NewArrayLiteral
        | Kind::LoadPropertyCached { .. }
        | Kind::StorePropertyCached { .. }
        | Kind::StoreKeyedCached { .. }
        | Kind::LoadKeyedCached { .. }
        | Kind::RegExpMethod { .. }
        | Kind::Instanceof
        | Kind::ArrayPush(_)
        | Kind::ArrayPop(_)
        | Kind::LooseEqual { .. }
        | Kind::CallJs { .. }
        | Kind::CallForward { .. }
        | Kind::Generic { .. } => Effects::world(),
        // Control.
        Kind::Jump(_) | Kind::JumpLoop(_) | Kind::Branch { .. } | Kind::Return | Kind::Deopt(_) => {
            Effects::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heaps_overlap_along_the_tree_only() {
        use AbstractHeap as H;
        assert!(H::World.overlaps(H::Field(3)));
        assert!(H::Fields.overlaps(H::Field(3)));
        assert!(H::Field(3).overlaps(H::Field(3)));
        assert!(!H::Field(3).overlaps(H::Field(4)));
        assert!(!H::Field(3).overlaps(H::MapWords));
        assert!(!H::Elements.overlaps(H::ArrayLengths));
        assert!(!H::ContextSlot(8).overlaps(H::ContextSlot(16)));
        assert!(H::Heap.overlaps(H::ContextSlot(16)));
        assert!(!H::Heap.overlaps(H::Movement));
        let mut set = HeapSet::default();
        set.add(H::Field(3));
        set.add(H::Fields);
        assert!(set.overlaps(H::Field(9)));
        assert!(!set.overlaps(H::MapWords));
        set.add(H::World);
        assert!(set.is_world());
    }

    #[test]
    fn a_cell_method_load_commutes_with_field_stores_but_not_appends() {
        let method = clobberize(&Kind::LoadGuardedMethod {
            byte_pc: 0,
            target: 0,
            receiver_proved: true,
            cell_read: true,
        });
        let slot_method = clobberize(&Kind::LoadGuardedMethod {
            byte_pc: 0,
            target: 0,
            receiver_proved: true,
            cell_read: false,
        });
        let writes = |kind: &Kind| {
            let mut set = HeapSet::default();
            for heap in clobberize(kind).writes {
                set.add(heap);
            }
            set
        };
        let overwrite = writes(&Kind::StoreOwnField {
            field: otter_vm::object::FieldLocation::inline(0),
            atom: 7,
            owners: Default::default(),
        });
        assert!(!method.reads_overlap(&overwrite));
        assert!(slot_method.reads_overlap(&overwrite));
        // A field-type check still sees the overwrite.
        assert!(clobberize(&Kind::CheckFieldTypes(Default::default())).reads_overlap(&overwrite));
        let append = writes(&Kind::StoreNamedProperty {
            byte_pc: 0,
            owners: Default::default(),
            receiver_shape: None,
            fresh: false,
        });
        assert!(method.reads_overlap(&append));
        assert!(clobberize(&Kind::CheckDependencies).reads_overlap(&overwrite));
    }
}
