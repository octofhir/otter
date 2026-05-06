//! Rooted locals are tied to an isolate handle scope and must not
//! cross Tokio worker boundaries.

fn spawn_local<'gc>(local: otter_gc::Local<'gc, ()>) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        std::hint::black_box(local);
    });
}

fn main() {
    let _ = spawn_local;
}
