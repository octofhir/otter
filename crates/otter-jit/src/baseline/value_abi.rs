//! Frozen JavaScript value encoding consumed by baseline machine code.
//!
//! # Contents
//! - VM-owned tag constants re-exported for templates and tests.
//! - Pre-split immediates used by AArch64 materialization sequences.
//! - Compile-time assertions that reject incompatible VM layout changes.
//!
//! # Invariants
//! Emitted code may bake only constants defined by the VM's public value-tag
//! contract. Changing that contract must fail this crate at compile time.
//!
//! # See also
//! - `otter_vm::value::tag` — authoritative boxed-value representation.

pub(crate) use otter_vm::value::tag as value_tag;

/// High 16 bits of [`value_tag::NUMBER_TAG`].
pub(crate) const NUMBER_TAG_HI16: u32 = (value_tag::NUMBER_TAG >> 48) as u32;
/// High 16 bits of [`value_tag::DOUBLE_ENCODE_OFFSET`].
pub(crate) const DOUBLE_OFFSET_HI16: u32 = (value_tag::DOUBLE_ENCODE_OFFSET >> 48) as u32;
/// High 16 bits of [`value_tag::CANONICAL_NAN`].
pub(crate) const CANONICAL_NAN_HI16: u32 = (value_tag::CANONICAL_NAN >> 48) as u32;
/// `null` immediate.
pub(crate) const VALUE_NULL: u64 = value_tag::VALUE_NULL;
/// `false` immediate.
pub(crate) const VALUE_FALSE: u64 = value_tag::VALUE_FALSE;
/// Low 32 bits of [`VALUE_FALSE`] for immediate materialization.
pub(crate) const VALUE_FALSE_LOW: u32 = value_tag::VALUE_FALSE as u32;
/// `true` immediate.
pub(crate) const VALUE_TRUE: u64 = value_tag::VALUE_TRUE;
/// `undefined` immediate.
pub(crate) const VALUE_UNDEFINED: u64 = value_tag::VALUE_UNDEFINED;
/// Internal hole sentinel.
pub(crate) const VALUE_HOLE: u64 = value_tag::VALUE_HOLE;
/// Low tag selecting a closure-less function-id immediate.
pub(crate) const FUNCTION_ID_TAG: u64 = value_tag::FUNCTION_ID_TAG;

const _: () = assert!(value_tag::NUMBER_TAG == 0xfffe_0000_0000_0000);
const _: () = assert!(value_tag::DOUBLE_ENCODE_OFFSET == 0x0002_0000_0000_0000);
const _: () = assert!(value_tag::NOT_CELL_MASK == value_tag::NUMBER_TAG | value_tag::OTHER_TAG);
const _: () = assert!(value_tag::CANONICAL_NAN == 0x7ff8_0000_0000_0000);
