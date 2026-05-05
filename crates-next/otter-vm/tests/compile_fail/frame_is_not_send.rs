//! `Frame` owns register windows and async/generator suspension
//! state containing `Value`, so parked JS frames must stay on the
//! isolate thread.
//!
//! Spec: task 82 / parked-frame root migration.

fn assert_send<T: Send>(_t: T) {}

fn main() {
    let function = otter_bytecode::Function::default();
    let frame = otter_vm::Frame::for_function(&function);
    assert_send(frame);
}
