//! `#[derive(Pelt)]` must reject bodies that forgot to declare a
//! `#[pelt(tag = …)]` attribute — otherwise the type would silently
//! coexist with another at the same default tag and corrupt the GC
//! dispatch table.

use otter_macros::Pelt;
use otter_vm::Value;

#[derive(Pelt)]
struct Body {
    field: Value,
}

fn main() {}
