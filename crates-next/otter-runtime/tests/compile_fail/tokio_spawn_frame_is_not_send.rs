//! Parked VM frames contain JS values and must not cross Tokio worker
//! boundaries.

fn main() {
    let function = otter_bytecode::Function::default();
    let frame = otter_vm::Frame::for_function(&function);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        std::hint::black_box(frame);
    });
}
