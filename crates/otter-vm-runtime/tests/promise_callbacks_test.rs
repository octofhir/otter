//! Tests for Promise.prototype.then/catch/finally with JS callbacks

use otter_vm_runtime::Otter;

#[tokio::test]
async fn test_promise_then_returns_value() {
    let mut otter = Otter::new();

    // Promise.then should return the value when awaited
    otter
        .eval(
            r#"
            await Promise.resolve(42).then(v => {
                if (v !== 42) throw new Error("Expected 42");
                return v;
            });
        "#,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn test_promise_then_callback_executes_with_console_log() {
    let mut otter = Otter::new();

    // Use console.log to verify callback executes
    otter
        .eval(
            r#"
            await Promise.resolve(100).then(v => {
                return v * 2;
            }).then(v => {
                if (v !== 200) throw new Error("Expected 200");
                return v;
            });
        "#,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn test_promise_catch_callback() {
    let mut otter = Otter::new();

    otter
        .eval(
            r#"
            await Promise.reject("test error").catch(e => {
                if (e !== "test error") throw new Error("Expected error");
                return "recovered";
            }).then(v => {
                if (v !== "recovered") throw new Error("Expected recovered");
                return v;
            });
        "#,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn test_promise_chaining() {
    let mut otter = Otter::new();

    otter
        .eval(
            r#"
            await Promise.resolve(1)
                .then(v => v + 1)
                .then(v => v + 1)
                .then(v => {
                    if (v !== 3) throw new Error("Expected 3");
                    return v;
                });
        "#,
        )
        .await
        .unwrap();
}
