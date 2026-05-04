//! `Local<'gc, T>` must not be `Send` — see ADR-0005 §3 / task 76A.

fn assert_send<T: Send>(_t: T) {}

fn main() {
    fn take_local<'gc>(local: otter_gc::Local<'gc, ()>) {
        assert_send(local);
    }
    let _ = take_local;
}
