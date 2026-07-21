//! Multiple isolates share host services without sharing JavaScript state.
//!
//! A browser process owns one Tokio host, gives every page its own runtime,
//! and delivers cross-page notifications as owned tasks to the target isolate.

use std::sync::mpsc::{Sender, channel};

use otter_runtime::{
    OtterError, Runtime, RuntimeLiveness, RuntimeTask, SourceInput, TokioRuntimeHost,
};

struct DeliverOwnedEvent {
    source_page: String,
    sequence: u64,
    delivered: Sender<()>,
}

impl RuntimeTask for DeliverOwnedEvent {
    fn run(self: Box<Self>, runtime: &mut Runtime) -> Result<(), OtterError> {
        // The task crosses the host boundary with owned Rust data only. A real
        // browser binding would materialize a StorageEvent/BroadcastChannel
        // Event here rather than evaluating source text.
        let source = format!(
            "globalThis.lastHostEvent = {{ sourcePage: {:?}, sequence: {} }};",
            self.source_page, self.sequence
        );
        runtime.eval(SourceInput::from_javascript(source))?;
        let _ = self.delivered.send(());
        Ok(())
    }
}

fn eval(host: &TokioRuntimeHost, runtime: &otter_runtime::RuntimeHandle, source: &str) -> String {
    host.handle()
        .block_on(runtime.eval(SourceInput::from_javascript(source)))
        .expect("script runs")
        .completion_string()
        .to_string()
}

#[test]
fn per_page_isolates_share_one_host_but_not_globals_or_heaps() {
    let host = TokioRuntimeHost::new().expect("shared Tokio host");
    let page_a = Runtime::builder()
        .runtime_host(host.clone())
        .build_handle()
        .expect("page A isolate");
    let page_b = Runtime::builder()
        .runtime_host(host.clone())
        .build_handle()
        .expect("page B isolate");

    assert_eq!(
        eval(
            &host,
            &page_a,
            "globalThis.pageIdentity = 'page-a'; pageIdentity",
        ),
        "page-a"
    );
    assert_eq!(
        eval(&host, &page_b, "typeof globalThis.pageIdentity"),
        "undefined",
        "a sibling page has a distinct global object and heap"
    );

    let (delivered_tx, delivered_rx) = channel();
    page_b
        .enqueue_runtime_task(
            DeliverOwnedEvent {
                source_page: "page-a".to_string(),
                sequence: 7,
                delivered: delivered_tx,
            },
            RuntimeLiveness::Ref,
        )
        .expect("browser host queues an event for page B");
    delivered_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("page B runs the event task");

    assert_eq!(
        eval(
            &host,
            &page_b,
            "lastHostEvent.sourcePage + ':' + lastHostEvent.sequence",
        ),
        "page-a:7"
    );
    assert_eq!(
        eval(&host, &page_a, "typeof globalThis.lastHostEvent"),
        "undefined",
        "delivery mutates only the target isolate"
    );

    page_b.shutdown();
    assert!(page_b.is_shutdown(), "page teardown is explicit");
    assert!(
        page_b
            .enqueue_runtime_task(
                DeliverOwnedEvent {
                    source_page: "too-late".to_string(),
                    sequence: 8,
                    delivered: channel().0,
                },
                RuntimeLiveness::Ref,
            )
            .is_err(),
        "events cannot be queued into a destroyed page"
    );
}
