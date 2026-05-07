//! Worker messages must not carry rooted locals.

fn send_local<'gc>(local: otter_gc::Local<'gc, ()>) {
    let worker = otter_runtime::Worker::new().unwrap();
    worker.accepts_message(&local);
}

fn main() {
    let _ = send_local;
}
