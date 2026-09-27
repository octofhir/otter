//! Scalar representation contract for indexed memory operations.
//!
//! # Contents
//! - Exact load representations and legal direct store operands.
//!
//! # Invariants
//! - Scalar loads preserve signedness and every Number bit, including -0/NaN.
//! - Tagged loads are the common result of committed fast/cold joins.
//! - Integer stores truncate low bits; unsigned clamped stores require their
//!   tagged conversion until the emitter has an unsigned clamp operation.
//! - A raw double store targets Float32/Float64 storage, never tagged payloads.
//!
//! # See also
//! - `numeric::element_cfg` selects guards before effects and roots cold calls.
//! - `mod.rs` verifies this contract before allocation and emission.

use super::MachineRepresentation as R;
use otter_vm::JitElementRepr as E;

pub(super) fn valid_load(element: E, result: R) -> bool {
    result == R::Tagged
        || result
            == match element {
                E::Boxed => R::Tagged,
                E::Uint32 => R::Uint32,
                E::Float32 | E::Float64 => R::Float64,
                _ => R::Int32,
            }
}

pub(super) fn valid_store(element: E, value: R) -> bool {
    match value {
        R::Tagged => true,
        R::Int32 => element.stores_int32(),
        R::Uint32 => element.stores_int32() && element != E::Uint8Clamped,
        R::Float64 => matches!(element, E::Float32 | E::Float64),
        _ => false,
    }
}
