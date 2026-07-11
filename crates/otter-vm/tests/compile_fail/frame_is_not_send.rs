//! `Frame` owns an active register window, so live JS frames must stay on the
//! isolate thread.


fn assert_send<T: Send>() {}

fn main() {
    assert_send::<otter_vm::Frame>();
}
