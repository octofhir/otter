//! `Value` may carry isolate-local GC handles, so it must not be
//! captured by a `Send + 'static` host future.
//!
//! Spec: task 82 / parked-frame root migration.

fn assert_send<T: Send>(_t: T) {}

fn main() {
    let value = otter_vm::Value::Undefined;
    assert_send(value);
}
