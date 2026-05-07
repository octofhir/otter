//! Raw GC handles are isolate-local and must not cross Tokio worker
//! boundaries.

fn main() {
    let handle: otter_gc::Gc<()> = otter_gc::Gc::null();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        std::hint::black_box(handle);
    });
}
