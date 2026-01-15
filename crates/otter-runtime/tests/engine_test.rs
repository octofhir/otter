//! Integration tests for the Engine API

use otter_runtime::{Engine, EngineHandle};
use serde_json::json;

#[tokio::test]
async fn test_basic_eval() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle.eval("1 + 1").await.unwrap();
    assert_eq!(result, json!(2));

    engine.shutdown().await;
}

#[tokio::test]
async fn test_eval_string() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle.eval("'hello' + ' ' + 'world'").await.unwrap();
    assert_eq!(result, json!("hello world"));

    engine.shutdown().await;
}

#[tokio::test]
async fn test_eval_object() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle.eval("({ a: 1, b: 2 })").await.unwrap();
    assert_eq!(result, json!({"a": 1, "b": 2}));

    engine.shutdown().await;
}

#[tokio::test]
async fn test_eval_array() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle.eval("[1, 2, 3].map(x => x * 2)").await.unwrap();
    assert_eq!(result, json!([2, 4, 6]));

    engine.shutdown().await;
}

#[tokio::test]
async fn test_handle_is_clone() {
    let engine = Engine::new().unwrap();
    let handle1 = engine.handle();
    let handle2 = handle1.clone();

    let r1 = handle1.eval("1").await.unwrap();
    let r2 = handle2.eval("2").await.unwrap();

    assert_eq!(r1, json!(1));
    assert_eq!(r2, json!(2));

    engine.shutdown().await;
}

#[tokio::test]
async fn test_concurrent_eval() {
    let engine = Engine::builder().pool_size(4).build().unwrap();
    let handle = engine.handle();

    // Spawn many concurrent evals
    let futures: Vec<_> = (0..100)
        .map(|i| {
            let h = handle.clone();
            tokio::spawn(async move { h.eval(format!("{}", i)).await })
        })
        .collect();

    for (i, future) in futures.into_iter().enumerate() {
        let result = future.await.unwrap().unwrap();
        assert_eq!(result, json!(i));
    }

    engine.shutdown().await;
}

#[tokio::test]
async fn test_error_handling() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle.eval("throw new Error('test error')").await;
    assert!(result.is_err());

    let err = result.unwrap_err();
    assert!(err.to_string().contains("test error"));

    engine.shutdown().await;
}

#[tokio::test]
async fn test_syntax_error() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle.eval("function(").await;
    assert!(result.is_err());

    engine.shutdown().await;
}

#[tokio::test]
async fn test_reference_error() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle.eval("nonexistent_variable").await;
    assert!(result.is_err());

    let err = result.unwrap_err();
    assert!(err.to_string().contains("ReferenceError") || err.to_string().contains("not defined"));

    engine.shutdown().await;
}

#[tokio::test]
async fn test_typescript_eval() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle
        .eval_typescript("const x: number = 42; x * 2")
        .await
        .unwrap();
    assert_eq!(result, json!(84));

    engine.shutdown().await;
}

#[tokio::test]
async fn test_typescript_interface() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle
        .eval_typescript(
            r#"
            interface User {
                name: string;
                age: number;
            }
            const user: User = { name: "Alice", age: 30 };
            user
            "#,
        )
        .await
        .unwrap();
    assert_eq!(result, json!({"name": "Alice", "age": 30}));

    engine.shutdown().await;
}

#[tokio::test]
async fn test_handle_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<EngineHandle>();
}

#[tokio::test]
async fn test_builder_pool_size() {
    let engine = Engine::builder().pool_size(2).build().unwrap();
    assert_eq!(engine.pool_size(), 2);
    engine.shutdown().await;
}

#[tokio::test]
async fn test_builder_queue_capacity() {
    let engine = Engine::builder()
        .pool_size(1)
        .queue_capacity(10)
        .build()
        .unwrap();

    let handle = engine.handle();

    // Should work normally
    let result = handle.eval("42").await.unwrap();
    assert_eq!(result, json!(42));

    engine.shutdown().await;
}

#[tokio::test]
async fn test_try_eval() {
    let engine = Engine::builder()
        .pool_size(1)
        .queue_capacity(10)
        .build()
        .unwrap();
    let handle = engine.handle();

    // Submit a job with try_eval
    let rx = handle.try_eval("1 + 2").unwrap();
    let result = rx.await.unwrap().unwrap();
    assert_eq!(result, json!(3));

    engine.shutdown().await;
}

#[tokio::test]
async fn test_eval_with_source() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle.eval_with_source("1 + 1", "test.js").await.unwrap();
    assert_eq!(result, json!(2));

    engine.shutdown().await;
}

#[tokio::test]
async fn test_is_running() {
    let engine = Engine::new().unwrap();
    assert!(engine.is_running());

    engine.shutdown().await;
    // After shutdown, the engine is dropped, so we can't check is_running
}

#[tokio::test]
async fn test_undefined_result() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle.eval("undefined").await.unwrap();
    assert_eq!(result, json!(null));

    engine.shutdown().await;
}

#[tokio::test]
async fn test_null_result() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle.eval("null").await.unwrap();
    assert_eq!(result, json!(null));

    engine.shutdown().await;
}

#[tokio::test]
async fn test_boolean_result() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle.eval("true").await.unwrap();
    assert_eq!(result, json!(true));

    let result = handle.eval("false").await.unwrap();
    assert_eq!(result, json!(false));

    engine.shutdown().await;
}

#[tokio::test]
async fn test_multiple_engines() {
    let engine1 = Engine::builder().pool_size(1).build().unwrap();
    let engine2 = Engine::builder().pool_size(1).build().unwrap();

    let handle1 = engine1.handle();
    let handle2 = engine2.handle();

    let r1 = handle1.eval("'engine1'").await.unwrap();
    let r2 = handle2.eval("'engine2'").await.unwrap();

    assert_eq!(r1, json!("engine1"));
    assert_eq!(r2, json!("engine2"));

    engine1.shutdown().await;
    engine2.shutdown().await;
}

#[tokio::test]
async fn test_engine_stats() {
    let engine = Engine::builder().pool_size(1).build().unwrap();
    let handle = engine.handle();

    // Initial stats should be zero
    let stats = engine.stats().snapshot();
    assert_eq!(stats.jobs_submitted, 0);
    assert_eq!(stats.jobs_completed, 0);
    assert_eq!(stats.jobs_failed, 0);

    // Run some successful evals
    handle.eval("1 + 1").await.unwrap();
    handle.eval("2 + 2").await.unwrap();
    handle.eval("3 + 3").await.unwrap();

    // Give workers time to update stats
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let stats = engine.stats().snapshot();
    assert_eq!(stats.jobs_submitted, 3);
    assert_eq!(stats.jobs_completed, 3);
    assert_eq!(stats.jobs_failed, 0);
    assert_eq!(stats.success_rate(), 100.0);

    // Run a failing eval
    let _ = handle.eval("throw new Error('test')").await;

    // Give workers time to update stats
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let stats = engine.stats().snapshot();
    assert_eq!(stats.jobs_submitted, 4);
    assert_eq!(stats.jobs_completed, 4);
    assert_eq!(stats.jobs_failed, 1);
    assert!(stats.success_rate() < 100.0);

    engine.shutdown().await;
}

#[tokio::test]
async fn test_handle_stats() {
    let engine = Engine::builder().pool_size(1).build().unwrap();
    let handle = engine.handle();

    // Run some evals
    handle.eval("1").await.unwrap();
    handle.eval("2").await.unwrap();

    // Give workers time to update stats
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Stats should be accessible from handle too
    let stats = handle.stats().snapshot();
    assert_eq!(stats.jobs_submitted, 2);
    assert_eq!(stats.jobs_completed, 2);

    engine.shutdown().await;
}

#[tokio::test]
async fn test_custom_tokio_handle() {
    // Get current tokio handle and pass it explicitly
    let tokio_handle = tokio::runtime::Handle::current();

    let engine = Engine::builder()
        .pool_size(1)
        .tokio_handle(tokio_handle)
        .build()
        .unwrap();

    let handle = engine.handle();
    let result = handle.eval("42").await.unwrap();
    assert_eq!(result, json!(42));

    engine.shutdown().await;
}

#[tokio::test]
async fn test_tokio_handle_captured_automatically() {
    // Engine::new() should automatically capture the current tokio handle
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    // Basic eval should work
    let result = handle.eval("1 + 1").await.unwrap();
    assert_eq!(result, json!(2));

    engine.shutdown().await;
}
