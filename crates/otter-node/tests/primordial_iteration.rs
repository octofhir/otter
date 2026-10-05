//! Runtime-internal code iterates with intrinsic algorithms.
//!
//! The Node library loads lazily, after user code may have replaced the
//! built-in iteration protocol. Like Node's primordials, its own `for…of`,
//! destructuring and spread must keep using the intrinsic iterators.

use otter_node::NodeApiBuilderExt;
use otter_runtime::{CapabilitySet, Runtime};

#[test]
fn library_code_ignores_replaced_array_iterator_next() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = dir.path().join("entry.cjs");
    std::fs::write(
        &entry,
        r#"
        const AIP = Object.getPrototypeOf([][Symbol.iterator]());
        const next = AIP.next;
        let calls = 0;
        AIP.next = function () { calls++; return { done: true }; };
        require("node:util").format("%s %d", "a", 1);
        console.log("primordial iteration");
        AIP.next = next;
        if (calls !== 0) throw new Error(`library iteration called the user next ${calls} times`);
        let [user] = [1];
        "#,
    )
    .expect("write fixture");
    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .expect("runtime with Node APIs");
    runtime.run_file(&entry).expect("fixture");
}
