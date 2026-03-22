//! FeedbackVector / IC state snapshot reader.
//!
//! Reads the interpreter's feedback data at compile time to guide
//! MIR specialization decisions: which guards to emit, what types
//! to specialize for, which call targets are monomorphic.
//!
//! The snapshot is taken once before compilation starts. If IC state
//! changes after this, the function will deopt and be recompiled.

use otter_vm_bytecode::Function;
use otter_vm_bytecode::function::{ArithmeticType, InlineCacheState, TypeFlags};

/// A compile-time snapshot of IC state for one feedback slot.
#[derive(Debug, Clone)]
pub enum IcSnapshot {
    /// No IC data available (uninitialized or not applicable).
    None,
    /// Monomorphic property access — single shape.
    MonoProp {
        shape_id: u64,
        offset: u32,
        depth: u8,
        proto_shape_id: u64,
    },
    /// Polymorphic property access — 2-4 shapes.
    PolyProp {
        entries: Vec<(u64, u32, u8)>, // (shape_id, offset, depth)
    },
    /// Megamorphic — too many shapes, no specialization possible.
    Megamorphic,
    /// Monomorphic call target.
    MonoCall { func_id: u64, jit_entry: u64 },
    /// Polymorphic call targets.
    PolyCall { entries: Vec<(u64, u64)> },
    /// Arithmetic type specialization.
    Arithmetic(ArithmeticType),
}

/// Compile-time snapshot of a function's full feedback state.
#[derive(Debug)]
pub struct FeedbackSnapshot {
    /// Per-IC-slot snapshots.
    pub slots: Vec<IcSnapshot>,
    /// Per-IC-slot type observations.
    pub type_flags: Vec<TypeFlags>,
}

impl FeedbackSnapshot {
    /// Take a snapshot of the function's current feedback state.
    pub fn from_function(function: &Function) -> Self {
        let feedback = function.feedback_vector.read();
        let len = feedback.len();

        let mut slots = Vec::with_capacity(len);
        let mut type_flags_vec = Vec::with_capacity(len);

        for i in 0..len {
            let entry = &feedback[i];
            let ic = convert_ic_state(&entry.ic_state);
            slots.push(ic);
            type_flags_vec.push(entry.type_observations);
        }

        Self {
            slots,
            type_flags: type_flags_vec,
        }
    }

    /// Get IC snapshot for a feedback slot.
    pub fn ic(&self, index: u16) -> &IcSnapshot {
        self.slots.get(index as usize).unwrap_or(&IcSnapshot::None)
    }

    /// Get type observations for a feedback slot.
    pub fn types(&self, index: u16) -> TypeFlags {
        self.type_flags
            .get(index as usize)
            .copied()
            .unwrap_or_default()
    }

    /// Whether a property IC slot is monomorphic for own-property access.
    pub fn is_mono_own_prop(&self, index: u16) -> bool {
        matches!(self.ic(index), IcSnapshot::MonoProp { depth: 0, .. })
    }

    /// Whether an arithmetic IC slot suggests Int32 specialization.
    pub fn is_int32_arithmetic(&self, index: u16) -> bool {
        matches!(
            self.ic(index),
            IcSnapshot::Arithmetic(ArithmeticType::Int32)
        )
    }

    /// Whether an arithmetic IC slot suggests Number (f64) specialization.
    pub fn is_number_arithmetic(&self, index: u16) -> bool {
        matches!(
            self.ic(index),
            IcSnapshot::Arithmetic(ArithmeticType::Number)
        )
    }

    /// Whether a call IC slot is monomorphic.
    pub fn is_mono_call(&self, index: u16) -> bool {
        matches!(self.ic(index), IcSnapshot::MonoCall { .. })
    }
}

fn convert_ic_state(state: &InlineCacheState) -> IcSnapshot {
    match state {
        InlineCacheState::Uninitialized => IcSnapshot::None,
        InlineCacheState::Monomorphic {
            shape_id,
            offset,
            depth,
            proto_shape_id,
        } => IcSnapshot::MonoProp {
            shape_id: *shape_id,
            offset: *offset,
            depth: *depth,
            proto_shape_id: *proto_shape_id,
        },
        InlineCacheState::Polymorphic { count, entries } => {
            let mut es = Vec::with_capacity(*count as usize);
            for i in 0..*count as usize {
                let (sid, _psid, depth, offset) = entries[i];
                es.push((sid, offset, depth));
            }
            IcSnapshot::PolyProp { entries: es }
        }
        InlineCacheState::Megamorphic => IcSnapshot::Megamorphic,
        InlineCacheState::MonoCall { func_id, jit_entry } => IcSnapshot::MonoCall {
            func_id: *func_id,
            jit_entry: *jit_entry,
        },
        InlineCacheState::PolyCall { count, entries } => {
            let mut es = Vec::with_capacity(*count as usize);
            for i in 0..*count as usize {
                es.push(entries[i]);
            }
            IcSnapshot::PolyCall { entries: es }
        }
        InlineCacheState::ArithmeticFastPath(ty) => IcSnapshot::Arithmetic(*ty),
    }
}
