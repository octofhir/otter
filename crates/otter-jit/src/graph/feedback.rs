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
//!   a guarded shape transition.
//!
//! # Invariants
//! - A site is speculated only when every installed program is understood;
//!   one unknown program keeps the whole site generic.
//! - Offsets are relative to the receiver's slot base (in-object or slab),
//!   exactly as the snapshot's programs state them.
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
    /// Byte offset of the slot from the receiver's slot base.
    pub(crate) offset: i32,
}

/// The own-data load the site at `byte_pc` observed, if every program is
/// one and all agree on the slot.
pub(crate) fn own_data_load(view: &JitCompileSnapshot, byte_pc: u32) -> Option<OwnDataLoad> {
    let programs = view.property_programs.get(&byte_pc)?;
    if programs.is_empty() || programs.len() > 4 {
        return None;
    }
    let mut shapes = SmallVec::new();
    let mut offset = None;
    for program in programs {
        let [
            JitCacheIrOp::GuardShape { object: 0, shape },
            JitCacheIrOp::GuardAtomSlot {
                object: 0,
                value_byte,
                writable: false,
                ..
            },
            JitCacheIrOp::LoadField {
                object: 0,
                value_byte: load_byte,
            },
        ] = &*program.ops
        else {
            return None;
        };
        if value_byte != load_byte {
            return None;
        }
        let byte = i32::try_from(*value_byte).ok()?;
        if offset.is_some_and(|known| known != byte) {
            return None;
        }
        offset = Some(byte);
        shapes.push(*shape);
    }
    Some(OwnDataLoad {
        shapes,
        offset: offset?,
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
    let mut offset = None;
    for program in programs {
        let [
            JitCacheIrOp::GuardShape { object: 0, shape },
            JitCacheIrOp::GuardAtomSlot {
                object: 0,
                value_byte,
                writable: true,
                ..
            },
            JitCacheIrOp::StoreField {
                object: 0,
                value_byte: store_byte,
            },
        ] = &*program.ops
        else {
            return None;
        };
        if value_byte != store_byte {
            return None;
        }
        let byte = i32::try_from(*value_byte).ok()?;
        if offset.is_some_and(|known| known != byte) {
            return None;
        }
        offset = Some(byte);
        shapes.push(*shape);
    }
    Some(OwnDataLoad {
        shapes,
        offset: offset?,
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
                JitCacheIrOp::GuardExtensible {
                    object: 0,
                    value_byte,
                } if !stored && value_byte % 8 == 0 => {}
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
