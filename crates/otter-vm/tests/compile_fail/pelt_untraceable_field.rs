//! `#[derive(Pelt)]` must reject fields whose type does not implement
//! `otter_vm::pelt::PeltField`. Authors are expected to either add the
//! impl or annotate the field with `#[pelt(skip)]`.

use otter_macros::Pelt;

struct ForeignType;

const TAG: u8 = 0xC9;

#[derive(Pelt)]
#[pelt(tag = TAG)]
struct Body {
    payload: ForeignType,
}

fn main() {}
