//! Parked VM frames contain JS values and must not cross Tokio worker
//! boundaries.

fn main() {
    let frame: Option<otter_vm::Frame> = None;
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        std::hint::black_box(frame);
    });
}
