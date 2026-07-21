use otter_runtime::{RuntimeHostValueSlot, RuntimeNativeScope};

struct BadPayload {
    callback: RuntimeHostValueSlot,
}

fn allocate_bad(scope: &mut RuntimeNativeScope<'_, '_>) {
    let _ = scope.host_object(BadPayload {
        callback: RuntimeHostValueSlot::empty(),
    });
}

fn main() {}
