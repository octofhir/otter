//! Native metadata borrows must end before a collecting context operation.
use otter_vm::NativeCtx;

fn retain(ctx: &mut NativeCtx<'_>) {
    let value = ctx.new_target().unwrap();
    let _ = ctx.alloc_object();
    println!("{value:?}");
}

fn main() {}
