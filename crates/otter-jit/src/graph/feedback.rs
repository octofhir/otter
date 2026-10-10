//! Reading the compile snapshot's per-site feedback as speculation the graph
//! builder can emit directly.
//!
//! # Contents
//! - [`OwnDataLoad`] / [`own_data_load`] — a named load whose selected
//!   receiver shapes keep the property as an own data slot at one location.
//! - [`own_data_store`] — the same for a store into an existing writable
//!   slot.
//! - [`NamedLoadPrograms`] / [`named_load_programs`] — source-owned indices
//!   of load programs compatible with the receiver's known ordinary shapes.
//!   Each reads the receiver, a guarded holder, a pinned intrinsic prototype,
//!   or a closure's own `prototype` slot.
//! - [`named_store_programs`] — any store whose every program writes an
//!   existing writable slot of an ordinary receiver or appends one through
//!   a guarded shape transition into its persistent inline capacity.
//!
//! # Invariants
//! - A load excludes a program only when its first receiver proof is
//!   incompatible with the known ordinary shapes. Without that knowledge,
//!   every installed program participates.
//! - Every selected program is understood; an unknown compatible program
//!   keeps the site generic. The four-program bound applies after selection.
//! - Program indices retain their order in the exact source snapshot.
//! - Single-slot specializations agree on the entire shape-owned FieldLocation:
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

/// Native load programs selected from one source-owned feedback bank.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamedLoadPrograms {
    /// Indices in the source snapshot's unmodified program bank.
    pub(crate) programs: SmallVec<[u32; 4]>,
    /// Ordinary shapes proved by the selected programs, when all start with
    /// an ordinary receiver shape guard.
    pub(crate) receiver_shapes: Option<SmallVec<[u32; 4]>>,
}

/// Select programs whose first receiver proof can match an ordinary object
/// of a known shape. An unrecognized proof stays selected for validation.
fn load_program_indices(
    programs: &[otter_vm::JitCacheIrProgram],
    receivers: Option<&[u32]>,
) -> Option<SmallVec<[u32; 4]>> {
    let mut selected = SmallVec::new();
    for (index, program) in programs.iter().enumerate() {
        if let Some(receivers) = receivers {
            match program.ops.first() {
                Some(JitCacheIrOp::GuardShape { object: 0, shape })
                    if !receivers.contains(shape) =>
                {
                    continue;
                }
                Some(JitCacheIrOp::LoadIntrinsicPrototype {
                    object: 0,
                    result: 1,
                    target,
                }) if target.is_generated_receiver()
                    && u32::from(target.type_tag) != crate::entry::OBJECT_BODY_TYPE_TAG =>
                {
                    // Admitted intrinsic body tags are disjoint from the
                    // ordinary object cells established by shape proofs.
                    continue;
                }
                Some(JitCacheIrOp::LoadClosurePrototype { object: 0 })
                    if program.ops.len() == 1 =>
                {
                    continue;
                }
                _ => {}
            }
        }
        if selected.len() == 4 {
            return None;
        }
        selected.push(u32::try_from(index).ok()?);
    }
    (!selected.is_empty()).then_some(selected)
}

/// The own-data load the site at `byte_pc` observed, if every selected
/// program reads one own slot and all agree on its complete location.
pub(crate) fn own_data_load(
    view: &JitCompileSnapshot,
    byte_pc: u32,
    receivers: Option<&[u32]>,
) -> Option<OwnDataLoad> {
    let programs = view.property_programs.get(&byte_pc)?;
    let selected = load_program_indices(programs, receivers)?;
    let mut shapes = SmallVec::new();
    let mut location = None;
    for index in selected {
        let program = &programs[index as usize];
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

/// The receiver shapes of a named site whose every program found the key
/// as the receiver's own data field: each shape fixes that the key is
/// present, whatever slot holds it.
pub(crate) fn own_key_shapes(
    view: &JitCompileSnapshot,
    byte_pc: u32,
) -> Option<SmallVec<[u32; 4]>> {
    let programs = view.property_programs.get(&byte_pc)?;
    if programs.is_empty() || programs.len() > 4 {
        return None;
    }
    programs
        .iter()
        .map(|program| match &*program.ops {
            [
                JitCacheIrOp::GuardShape { object: 0, shape },
                JitCacheIrOp::GuardAtomSlot { object: 0, .. },
                JitCacheIrOp::LoadField { object: 0, .. },
            ] => Some(*shape),
            _ => None,
        })
        .collect()
}

/// The existing writable own data slot every receiver shape the store site
/// at `byte_pc` observed writes, if all agree on its offset. With
/// `receivers`, the shapes the receiver is already proved to have, only the
/// programs of those shapes count.
pub(crate) fn own_data_store(
    view: &JitCompileSnapshot,
    byte_pc: u32,
    receivers: Option<&[u32]>,
) -> Option<OwnDataLoad> {
    let programs = view.property_programs.get(&byte_pc)?;
    if programs.is_empty() || programs.len() > 4 {
        return None;
    }
    let mut shapes = SmallVec::new();
    let mut location = None;
    for program in programs {
        if let (Some(receivers), Some(JitCacheIrOp::GuardShape { object: 0, shape })) =
            (receivers, program.ops.first())
            && !receivers.contains(shape)
        {
            continue;
        }
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

/// The selected load programs of the site at `byte_pc`, when every one
/// proves its receiver and reads one data slot through generated operations:
/// the receiver (operand 0) or its holder (operand 1) — a guarded prototype
/// holder or a pinned intrinsic prototype — ending in one field read, or a
/// closure's own `prototype` slot.
pub(crate) fn named_load_programs(
    view: &JitCompileSnapshot,
    byte_pc: u32,
    receivers: Option<&[u32]>,
) -> Option<NamedLoadPrograms> {
    let programs = view.property_programs.get(&byte_pc)?;
    let selected = load_program_indices(programs, receivers)?;
    for &index in &selected {
        let program = &programs[index as usize];
        // A closure's own `prototype` slot read stands alone.
        if let [JitCacheIrOp::LoadClosurePrototype { object: 0 }] = *program.ops {
            continue;
        }
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
    let receiver_shapes = selected
        .iter()
        .map(|&index| match programs[index as usize].ops.first() {
            Some(JitCacheIrOp::GuardShape { object: 0, shape }) => Some(*shape),
            _ => None,
        })
        .collect();
    Some(NamedLoadPrograms {
        programs: selected,
        receiver_shapes,
    })
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

    fn own_program(shape: u32, field: FieldLocation) -> otter_vm::JitCacheIrProgram {
        otter_vm::JitCacheIrProgram {
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
        }
    }

    fn string_program() -> otter_vm::JitCacheIrProgram {
        otter_vm::JitCacheIrProgram {
            ops: vec![
                JitCacheIrOp::LoadIntrinsicPrototype {
                    object: 0,
                    result: 1,
                    target: otter_vm::jit::JitIntrinsicPrototype {
                        type_tag: otter_vm::string::JS_STRING_BODY_TYPE_TAG,
                        guard: None,
                        proto_offset: 8,
                        active_realm: None,
                    },
                },
                JitCacheIrOp::GuardDictionaryLayout {
                    object: 1,
                    layout: 7,
                },
                JitCacheIrOp::GuardAtomSlot {
                    object: 1,
                    atom: 1,
                    field: FieldLocation::inline(0),
                    writable: false,
                },
                JitCacheIrOp::LoadField {
                    object: 1,
                    field: FieldLocation::inline(0),
                },
            ]
            .into_boxed_slice(),
        }
    }

    #[test]
    fn equal_displacements_in_different_banks_are_not_one_own_slot() {
        let mut view = JitCompileSnapshot::without_feedback(1, 0, 1, Vec::new());
        view.property_programs.insert(
            0,
            vec![
                own_program(8, FieldLocation::inline(0)),
                own_program(16, FieldLocation::overflow(0)),
            ],
        );
        assert!(own_data_load(&view, 0, None).is_none());
        view.property_programs.insert(
            0,
            vec![
                own_program(8, FieldLocation::inline(0)),
                own_program(16, FieldLocation::inline(0)),
            ],
        );
        assert_eq!(
            own_data_load(&view, 0, None).expect("same bank").field,
            FieldLocation::inline(0)
        );
    }

    #[test]
    fn known_shapes_select_before_program_count_and_field_location() {
        let mut view = JitCompileSnapshot::without_feedback(1, 0, 1, Vec::new());
        view.property_programs.insert(
            0,
            (1..=5)
                .map(|index| {
                    own_program(
                        index * 8,
                        if index == 2 {
                            FieldLocation::overflow(0)
                        } else {
                            FieldLocation::inline(0)
                        },
                    )
                })
                .collect(),
        );
        assert!(own_data_load(&view, 0, None).is_none());
        assert!(named_load_programs(&view, 0, None).is_none());
        assert_eq!(
            own_data_load(&view, 0, Some(&[16])).unwrap(),
            OwnDataLoad {
                shapes: smallvec::smallvec![16],
                field: FieldLocation::overflow(0),
            }
        );
        assert_eq!(
            named_load_programs(&view, 0, Some(&[16, 48])).unwrap(),
            NamedLoadPrograms {
                programs: smallvec::smallvec![1],
                receiver_shapes: Some(smallvec::smallvec![16]),
            }
        );
        assert!(own_data_load(&view, 0, Some(&[48])).is_none());
        assert!(named_load_programs(&view, 0, Some(&[48])).is_none());
    }

    #[test]
    fn ordinary_shapes_exclude_intrinsic_and_closure_programs_only_when_proved() {
        let mut view = JitCompileSnapshot::without_feedback(1, 0, 1, Vec::new());
        view.property_programs.insert(
            0,
            vec![
                own_program(8, FieldLocation::inline(0)),
                string_program(),
                otter_vm::JitCacheIrProgram {
                    ops: Box::new([JitCacheIrOp::LoadClosurePrototype { object: 0 }]),
                },
                own_program(16, FieldLocation::overflow(0)),
            ],
        );
        assert_eq!(
            named_load_programs(&view, 0, None).unwrap(),
            NamedLoadPrograms {
                programs: smallvec::smallvec![0, 1, 2, 3],
                receiver_shapes: None,
            }
        );
        assert!(own_data_load(&view, 0, None).is_none());
        assert_eq!(
            named_load_programs(&view, 0, Some(&[16])).unwrap(),
            NamedLoadPrograms {
                programs: smallvec::smallvec![3],
                receiver_shapes: Some(smallvec::smallvec![16]),
            }
        );
        assert_eq!(
            own_data_load(&view, 0, Some(&[16])).unwrap().field,
            FieldLocation::overflow(0)
        );
        assert!(named_load_programs(&view, 0, Some(&[24])).is_none());
    }

    #[test]
    fn unknown_compatible_programs_forbid_specialization() {
        let mut view = JitCompileSnapshot::without_feedback(1, 0, 1, Vec::new());
        let unknown = otter_vm::JitCacheIrProgram {
            ops: Box::new([
                JitCacheIrOp::GuardShape {
                    object: 0,
                    shape: 16,
                },
                JitCacheIrOp::GuardPrototypeNull { object: 0 },
                JitCacheIrOp::LoadField {
                    object: 0,
                    field: FieldLocation::inline(0),
                },
            ]),
        };
        view.property_programs
            .insert(0, vec![unknown, own_program(8, FieldLocation::inline(0))]);
        assert!(named_load_programs(&view, 0, None).is_none());
        assert!(named_load_programs(&view, 0, Some(&[16])).is_none());
        assert!(own_data_load(&view, 0, Some(&[16])).is_none());
        assert_eq!(
            named_load_programs(&view, 0, Some(&[8]))
                .unwrap()
                .programs
                .as_slice(),
            &[1]
        );

        let mut unknown_intrinsic = string_program();
        let JitCacheIrOp::LoadIntrinsicPrototype { target, .. } = &mut unknown_intrinsic.ops[0]
        else {
            unreachable!();
        };
        target.type_tag = crate::entry::OBJECT_BODY_TYPE_TAG as u8;
        view.property_programs.insert(
            0,
            vec![own_program(8, FieldLocation::inline(0)), unknown_intrinsic],
        );
        assert!(named_load_programs(&view, 0, Some(&[8])).is_none());
        assert!(own_data_load(&view, 0, Some(&[8])).is_none());
    }
}
