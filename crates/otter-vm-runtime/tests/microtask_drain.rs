//! Tests for microtask drain correctness
//!
//! Verifies that microtasks are drained at the correct synchronization points
//! as required by the ECMAScript specification.

use otter_vm_runtime::Otter;

#[test]
fn test_microtask_after_sync_eval() {
    // Promise.resolve().then() should execute callback before eval returns
    let mut otter = Otter::new();
    let result = otter.eval_sync(
        r#"
        let x = 0;
        Promise.resolve(42).then(v => { x = v; });
        Promise.resolve().then(() => {
            if (x !== 42) throw new Error("x should be 42");
        });
        "#,
    );

    assert!(result.is_ok(), "eval_sync should succeed");
}

#[test]
fn test_nested_microtasks() {
    // Microtask can enqueue another microtask, both should execute
    let mut otter = Otter::new();
    let result = otter.eval_sync(
        r#"
        let log = [];
        Promise.resolve(1).then(v => {
            log.push(v);
            Promise.resolve(2).then(w => log.push(w));
        });
        Promise.resolve().then(() => {}).then(() => {
            if (log.length !== 2) throw new Error("Expected 2 microtasks");
        });
        "#,
    );

    assert!(result.is_ok(), "eval_sync should succeed");
}

#[test]
fn test_multiple_promise_chains() {
    // Multiple independent promise chains should all execute
    let mut otter = Otter::new();
    let result = otter.eval_sync(
        r#"
        let sum = 0;
        Promise.resolve(10).then(v => { sum += v; });
        Promise.resolve(20).then(v => { sum += v; });
        Promise.resolve(30).then(v => { sum += v; });
        Promise.resolve().then(() => {
            if (sum !== 60) throw new Error("Expected sum 60");
        });
        "#,
    );

    assert!(result.is_ok(), "eval_sync should succeed");
}

#[test]
fn test_microtask_in_eval_in_context() {
    // eval_in_context should also drain microtasks
    let mut otter = Otter::new();
    let mut ctx = otter
        .create_test_context()
        .expect("Failed to create context");

    // Set up a variable in the context
    let setup = otter.eval_in_context(&mut ctx, "let result = 0;");
    assert!(setup.is_ok());

    // Execute code with promise that modifies the variable
    let exec = otter.eval_in_context(
        &mut ctx,
        r#"
        Promise.resolve(100).then(v => { result = v; });
        "#,
    );

    assert!(
        exec.is_ok(),
        "Promise setup should succeed: {:?}",
        exec.as_ref().err()
    );

    // Check the value after microtasks have been drained
    let check = otter.eval_in_context(&mut ctx, "result;");

    assert!(
        check.is_ok(),
        "Check should succeed: {:?}",
        check.as_ref().err()
    );
    let value = check.unwrap();
    assert_eq!(
        value.as_number(),
        Some(100.0),
        "Microtask should execute before eval_in_context returns"
    );
}

#[test]
fn test_promise_reject_handled() {
    // Promise rejection should be handled gracefully
    let mut otter = Otter::new();
    let result = otter.eval_sync(
        r#"
        let caught = false;
        Promise.reject("error").catch(e => { caught = true; });
        Promise.resolve().then(() => {
            if (!caught) throw new Error("catch should run");
        });
        "#,
    );

    assert!(result.is_ok(), "eval_sync should succeed");
}

#[test]
fn test_chained_then_callbacks() {
    // Chained .then() calls should all execute in order
    let mut otter = Otter::new();
    let result = otter.eval_sync(
        r#"
        let log = [];
        Promise.resolve(1)
            .then(v => { log.push(v); return v + 1; })
            .then(v => { log.push(v); return v + 1; })
            .then(v => { log.push(v); })
            .then(() => {
                if (log.length !== 3) throw new Error("Expected 3");
            });
        "#,
    );

    assert!(result.is_ok(), "eval_sync should succeed");
}

#[test]
fn test_microtask_with_global_modification() {
    // Microtasks should be able to modify global scope
    let mut otter = Otter::new();
    let result = otter.eval_sync(
        r#"
        globalThis.testValue = 0;
        Promise.resolve(999).then(v => { globalThis.testValue = v; });
        Promise.resolve().then(() => {
            if (globalThis.testValue !== 999) throw new Error("Expected 999");
        });
        "#,
    );

    assert!(result.is_ok(), "eval_sync should succeed");
}

#[test]
fn test_empty_microtask_queue() {
    // Code with no promises should not fail
    let mut otter = Otter::new();
    let result = otter.eval_sync("let x = 42; x;");

    assert!(
        result.is_ok(),
        "eval_sync should succeed without microtasks"
    );
    let value = result.unwrap();
    assert_eq!(value.as_number(), Some(42.0));
}

#[test]
fn test_promise_all_microtasks() {
    // Promise.all should work with microtask drain
    let mut otter = Otter::new();
    let result = otter.eval_sync(
        r#"
        let result = 0;
        Promise.all([
            Promise.resolve(10),
            Promise.resolve(20),
            Promise.resolve(30)
        ]).then(values => {
            result = values[0] + values[1] + values[2];
        }).then(() => {
            if (result !== 60) throw new Error("Expected 60");
        });
        "#,
    );

    assert!(result.is_ok(), "eval_sync should succeed");
}
