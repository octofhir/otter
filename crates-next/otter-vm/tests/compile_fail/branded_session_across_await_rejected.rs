//! Branded GC sessions must not cross async send boundaries.

fn assert_send_static_future<F>(_future: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
}

fn spawn_session(mut heap: otter_gc::GcHeap) {
    otter_gc::with_gc_session(&mut heap, |session| {
        assert_send_static_future(async move {
            std::future::pending::<()>().await;
            std::hint::black_box(session);
        });
    });
}

fn main() {
    let _ = spawn_session;
}
