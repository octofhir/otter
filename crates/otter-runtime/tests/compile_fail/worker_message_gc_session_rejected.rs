//! Worker messages must not carry branded GC sessions.

fn send_session(session: otter_gc::GcSession<'_, '_>) {
    let worker = otter_runtime::Worker::new().unwrap();
    worker.accepts_message(&session);
}

fn main() {
    let _ = send_session;
}
