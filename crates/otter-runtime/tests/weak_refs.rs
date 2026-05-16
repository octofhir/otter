//! Runtime regressions for WeakRef and FinalizationRegistry constructor paths.

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<test>")
        .expect("script")
        .completion_string()
        .to_string()
}

#[test]
fn weak_ref_and_finalization_registry_native_constructors_work() {
    let completion = run(r#"
        const target = { alive: true };
        const weak = new WeakRef(target);
        const registry = new FinalizationRegistry(function(value) {});
        registry.register(target, "held", target);
        const removed = registry.unregister(target);
        (weak.deref() === target) + ":" + removed;
        "#);
    assert_eq!(completion, "true:true");
}
