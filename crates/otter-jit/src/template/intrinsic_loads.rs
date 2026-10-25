//! Which of a baseline named-load site's property programs are static
//! intrinsic reads its code performs before the site's IC.
//!
//! # Contents
//! - [`IntrinsicLoad`] — one such read.
//! - [`intrinsic_loads`] — the reads a site's programs describe.
//!
//! # Invariants
//! - Every read proves a receiver that is not an ordinary object, so a site
//!   with any read skips them all at once for an immediate or an ordinary
//!   object, and a site with none emits nothing before its IC.
//! - A method lookup reads only for a primitive string or a RegExp: the
//!   committed resolution of any other receiver records call feedback the
//!   optimizer consumes.
//!
//! # See also
//! - `super::arm64::properties` and `super::x86_64` emit the reads.

use otter_vm::JitCacheIrOp as Op;
use otter_vm::JitCompileSnapshot;
use smallvec::SmallVec;

/// One static intrinsic read.
#[derive(Debug, Clone, Copy)]
pub(crate) enum IntrinsicLoad<'p> {
    /// A closure's own `prototype` slot.
    ClosurePrototype,
    /// An exotic body's own unguarded data slot.
    BodySlot(otter_vm::jit::JitBodySlot),
    /// A data slot of a pinned realm prototype or an array's expando object:
    /// `projection` selects the holder, `guards` prove it and `field` names
    /// the slot.
    Holder {
        projection: &'p Op,
        guards: &'p [Op],
        field: otter_vm::object::FieldLocation,
    },
}

/// The static intrinsic reads of the load site at `byte_pc`, in program
/// order; `method_lookup` keeps only those whose receivers record no call
/// feedback.
pub(crate) fn intrinsic_loads(
    view: &JitCompileSnapshot,
    byte_pc: u32,
    method_lookup: bool,
) -> SmallVec<[IntrinsicLoad<'_>; 2]> {
    let Some(programs) = view.property_programs.get(&byte_pc) else {
        return SmallVec::new();
    };
    programs
        .iter()
        .filter(|program| !method_lookup || feedback_free_method_program(&program.ops))
        .filter_map(|program| classify(&program.ops))
        .collect()
}

/// Whether `ops` read a method for a receiver whose committed resolution
/// records no call feedback: a primitive string or a RegExp.
pub(crate) fn feedback_free_method_program(ops: &[Op]) -> bool {
    matches!(
        ops.first(),
        Some(Op::LoadIntrinsicPrototype { target, .. })
            if target.type_tag == otter_vm::string::JS_STRING_BODY_TYPE_TAG
                || target.type_tag == otter_vm::regexp::REGEXP_BODY_TYPE_TAG
    )
}

fn classify(ops: &[Op]) -> Option<IntrinsicLoad<'_>> {
    if let [Op::LoadClosurePrototype { object: 0 }] = *ops {
        return Some(IntrinsicLoad::ClosurePrototype);
    }
    if let [Op::LoadBodySlot { object: 0, slot }] = *ops {
        return (slot.is_generated(false) && slot.guard.is_none())
            .then_some(IntrinsicLoad::BodySlot(slot));
    }
    let (projection, rest) = ops.split_first()?;
    if !matches!(
        projection,
        Op::LoadIntrinsicPrototype {
            object: 0,
            result: 1,
            ..
        } | Op::LoadArrayExpando {
            object: 0,
            result: 1,
            ..
        }
    ) {
        return None;
    }
    let (&Op::LoadField { object: 1, field }, guards) = rest.split_last()? else {
        return None;
    };
    guards
        .iter()
        .all(|op| {
            matches!(
                op,
                Op::GuardShape { object: 1, .. }
                    | Op::GuardDictionaryLayout { object: 1, .. }
                    | Op::GuardAtomSlot {
                        object: 1,
                        writable: false,
                        ..
                    }
                    | Op::GuardPrototypeValidity { .. }
            )
        })
        .then_some(IntrinsicLoad::Holder {
            projection,
            guards,
            field,
        })
}
