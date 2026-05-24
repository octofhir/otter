//! `#[derive(Pelt)]` only supports structs; enums must keep a
//! hand-written `SafeTraceable` impl so each variant's traced slots
//! stay explicit.

use otter_macros::Pelt;
use otter_vm::Value;

const TAG: u8 = 0xCA;

#[derive(Pelt)]
#[pelt(tag = TAG)]
enum Body {
    A(Value),
    B,
}

fn main() {}
