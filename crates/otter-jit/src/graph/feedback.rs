//! Reading the compile snapshot's per-site feedback as speculation the graph
//! builder can emit directly.
//!
//! # Contents
//! - [`OwnDataLoad`] / [`own_data_load`] — a named load whose every observed
//!   receiver shape keeps the property as an own data slot at one offset.
//! - [`own_data_store`] — the same for a store into an existing writable
//!   slot.
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
