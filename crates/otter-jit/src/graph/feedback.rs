//! Reading the compile snapshot's per-site feedback as speculation the graph
//! builder can emit directly.
//!
//! # Contents
//! - [`OwnDataLoad`] / [`own_data_load`] — a named load whose every observed
//!   receiver shape keeps the property as an own data slot at one offset.
//! - [`own_data_store`] — the same for a store into an existing writable
//!   slot.
//! - [`named_load_programs`] — any load whose every program reads one data
//!   slot of the receiver, of a guarded prototype holder, or of a pinned
//!   intrinsic prototype.
//! - [`named_store_programs`] — any store whose every program writes an
//!   existing writable slot of an ordinary receiver or appends one through
//!   a guarded shape transition into its persistent inline capacity.
//!
//! # Invariants
//! - A site is speculated only when every installed program is understood;
//!   one unknown program keeps the whole site generic.
//! - Every accepted program agrees on the entire shape-owned FieldLocation:
//!   both storage bank and relative word index, not only the byte displacement.
//!
//! # See also
//! - `otter_vm::jit::JitCacheIrProgram` — the program format read here.
//! - [`super::builder`] — the consumer.

use otter_vm::JitCompileSnapshot;
use otter_vm::jit::JitCacheIrOp;
use smallvec::SmallVec;

/// A named load from one own data slot of up to four receiver shapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnDataLoad {
    /// Compressed shape handles.
    pub(crate) shapes: SmallVec<[u32; 4]>,
    /// Immutable storage bank and bank-relative word index.
    pub(crate) field: otter_vm::object::FieldLocation,
}

/// The own-data load the site at `byte_pc` observed, if every program is
/// one and all agree on the slot.
pub(crate) fn own_data_load(view: &JitCompileSnapshot, byte_pc: u32) -> Option<OwnDataLoad> {
    let programs = view.property_programs.get(&byte_pc)?;
    if programs.is_empty() || programs.len() > 4 {
        return None;
    }
    let mut shapes = SmallVec::new();
    let mut location = None;
    for program in programs {
        let [
            JitCacheIrOp::GuardShape { object: 0, shape },
            JitCacheIrOp::GuardAtomSlot {
                object: 0,
                field,
                writable: false,
                ..
            },
            JitCacheIrOp::LoadField {
                object: 0,
                field: load_byte,
            },
        ] = &*program.ops
        else {
            return None;
        };
        if field != load_byte {
            return None;
        }
        i32::try_from(field.byte_offset()).ok()?;
        if location.is_some_and(|known| known != *field) {
            return None;
        }
        location = Some(*field);
        shapes.push(*shape);
    }
    Some(OwnDataLoad {
        shapes,
        field: location?,
    })
}

/// The existing writable own data slot every receiver shape the store site
/// at `byte_pc` observed writes, if all agree on its offset.
pub(crate) fn own_data_store(view: &JitCompileSnapshot, byte_pc: u32) -> Option<OwnDataLoad> {
    let programs = view.property_programs.get(&byte_pc)?;
    if programs.is_empty() || programs.len() > 4 {
        return None;
    }
    let mut shapes = SmallVec::new();
    let mut location = None;
    for program in programs {
        let [
            JitCacheIrOp::GuardShape { object: 0, shape },
            JitCacheIrOp::GuardAtomSlot {
                object: 0,
                field,
                writable: true,
                ..
            },
            JitCacheIrOp::StoreField {
                object: 0,
                field: store_byte,
            },
        ] = &*program.ops
        else {
            return None;
        };
        if field != store_byte {
            return None;
        }
        i32::try_from(field.byte_offset()).ok()?;
        if location.is_some_and(|known| known != *field) {
            return None;
        }
        location = Some(*field);
        shapes.push(*shape);
    }
    Some(OwnDataLoad {
        shapes,
        field: location?,
    })
}

/// The load programs of the site at `byte_pc`, when every one proves its
/// receiver and reads one data slot through operations generated code runs:
/// the receiver (operand 0) or its holder (operand 1) — a guarded prototype
/// holder or a pinned intrinsic prototype — ending in one field read.
pub(crate) fn named_load_programs(
    view: &JitCompileSnapshot,
    byte_pc: u32,
) -> Option<&[otter_vm::JitCacheIrProgram]> {
    let programs = view.property_programs.get(&byte_pc)?;
    if programs.is_empty() || programs.len() > 4 {
        return None;
    }
    for program in programs {
        let mut holder = false;
        let (last, guards) = program.ops.split_last()?;
        let JitCacheIrOp::LoadField { object, .. } = *last else {
            return None;
        };
        for (index, op) in guards.iter().enumerate() {
            match *op {
                JitCacheIrOp::LoadIntrinsicPrototype {
                    object: 0,
                    result: 1,
                    target,
                } if index == 0 && target.is_generated_receiver() => holder = true,
                JitCacheIrOp::LoadPrototypeHolder { result: 1, .. } => holder = true,
                JitCacheIrOp::GuardPrototypeValidity { .. } => {}
                JitCacheIrOp::GuardShape { object, .. }
                    if object == 0 || (object == 1 && holder) => {}
                JitCacheIrOp::GuardDictionaryLayout { object: 1, .. } if holder => {}
                JitCacheIrOp::GuardAtomSlot {
                    object,
                    writable: false,
                    ..
                } if object == 0 || (object == 1 && holder) => {}
                _ => return None,
            }
        }
        if object > 1 || (object == 1 && !holder) {
            return None;
        }
        // An ordinary receiver must be proved before its field is read.
        let intrinsic = matches!(
            program.ops.first(),
            Some(JitCacheIrOp::LoadIntrinsicPrototype { .. })
        );
        if !intrinsic
            && !matches!(
                program.ops.first(),
                Some(JitCacheIrOp::GuardShape { object: 0, .. })
            )
        {
            return None;
        }
    }
    Some(programs)
}

/// The store programs of the site at `byte_pc`, when every one proves an
/// ordinary receiver by shape and either writes an existing writable own
/// slot, or appends the slot within the receiver's storage and publishes the
/// child shape, with its prototype chain proved by validity cell, holder
/// shapes or a null link.
///
/// An overflow append may need to allocate or grow the suffix slab: a
/// receiver shape alone does not prove resident storage. The committed cached
/// store owns that collecting miss; the noncollecting named specialization
/// accepts only inline appends. Existing suffix-slot overwrites still
/// specialize directly.
pub(crate) fn named_store_programs(
    view: &JitCompileSnapshot,
    byte_pc: u32,
) -> Option<&[otter_vm::JitCacheIrProgram]> {
    let programs = view.property_programs.get(&byte_pc)?;
    if programs.is_empty() || programs.len() > 4 {
        return None;
    }
    for program in programs {
        if !matches!(
            program.ops.first(),
            Some(JitCacheIrOp::GuardShape { object: 0, .. })
        ) {
            return None;
        }
        let mut holder = false;
        let mut stored = false;
        let mut published = false;
        for op in program.ops.iter() {
            if published {
                return None;
            }
            match *op {
                JitCacheIrOp::LoadPrototypeHolder { result: 1, .. } if !stored => holder = true,
                JitCacheIrOp::GuardPrototypeValidity { .. } if !stored => {}
                JitCacheIrOp::GuardShape { object, .. }
                    if !stored && (object == 0 || (object == 1 && holder)) => {}
                JitCacheIrOp::GuardPrototypeNull { object }
                    if !stored && (object == 0 || (object == 1 && holder)) => {}
                JitCacheIrOp::GuardAtomSlot {
                    object: 0,
                    writable: true,
                    ..
                } if !stored => {}
                JitCacheIrOp::GuardExtensible { object: 0, field }
                    if !stored && field.is_inline() => {}
                JitCacheIrOp::StoreField { object: 0, .. } if !stored => stored = true,
                JitCacheIrOp::PublishShape { object: 0, .. } if stored => published = true,
                _ => return None,
            }
        }
        if !stored {
            return None;
        }
    }
    Some(programs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm::object::FieldLocation;

    #[test]
    fn equal_displacements_in_different_banks_are_not_one_own_slot() {
        let mut view = JitCompileSnapshot::without_feedback(1, 0, 1, Vec::new());
        let program = |shape, field| otter_vm::JitCacheIrProgram {
            ops: vec![
                JitCacheIrOp::GuardShape { object: 0, shape },
                JitCacheIrOp::GuardAtomSlot {
                    object: 0,
                    atom: 1,
                    field,
                    writable: false,
                },
                JitCacheIrOp::LoadField { object: 0, field },
            ]
            .into_boxed_slice(),
        };
        view.property_programs.insert(
            0,
            vec![
                program(8, FieldLocation::inline(0)),
                program(16, FieldLocation::overflow(0)),
            ],
        );
        assert!(own_data_load(&view, 0).is_none());
        view.property_programs.insert(
            0,
            vec![
                program(8, FieldLocation::inline(0)),
                program(16, FieldLocation::inline(0)),
            ],
        );
        assert_eq!(
            own_data_load(&view, 0).expect("same bank").field,
            FieldLocation::inline(0)
        );
    }
}
