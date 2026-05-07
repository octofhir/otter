//! `Value` may carry isolate-local GC handles, so Tokio host futures
//! must not capture it.

fn main() {
    let value = otter_vm::Value::Undefined;
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        std::hint::black_box(value);
    });
}
