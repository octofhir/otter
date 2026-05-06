//! Worker messages must be structured-clone payloads, not VM values.

fn main() {
    let worker = otter_runtime::Worker::new().unwrap();
    let value = otter_vm::Value::Undefined;
    worker.accepts_message(&value);
}
