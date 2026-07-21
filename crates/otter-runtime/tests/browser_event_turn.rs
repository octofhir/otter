//! Host-driven browser events are synchronous tasks with one final checkpoint.

use otter_runtime::{NativeCtx, NativeError, Runtime, SourceInput, Value};

fn dispatch_nested_listener(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let listener = ctx
        .global_value("innerListener")
        .unwrap_or_else(Value::undefined);
    ctx.call(listener, Value::undefined(), &[])
}

#[test]
fn nested_dispatch_stays_synchronous_and_microtasks_wait_for_task_end() {
    let mut runtime = Runtime::builder().build().expect("runtime");
    runtime
        .install_native_global("dispatchNested", 0, dispatch_nested_listener)
        .expect("native installs");
    let (_, context) = runtime
        .run_script_with_context(
            SourceInput::from_javascript(
                r#"
                globalThis.order = [];
                globalThis.innerListener = () => {
                    order.push("inner");
                    queueMicrotask(() => order.push("inner-microtask"));
                };
                globalThis.outerListener = () => {
                    order.push("outer-start");
                    queueMicrotask(() => order.push("outer-microtask"));
                    dispatchNested();
                    order.push("outer-end");
                };
                "#,
            ),
            "https://browser.test/page.js",
        )
        .expect("page script");

    runtime
        .run_native_event(&context, |ctx| {
            let listener = ctx
                .global_value("outerListener")
                .unwrap_or_else(Value::undefined);
            ctx.call(listener, Value::undefined(), &[])
        })
        .expect("event task");

    let result = runtime
        .eval(SourceInput::from_javascript("order.join(',')"))
        .expect("read order");
    assert_eq!(
        result.completion_string(),
        "outer-start,inner,outer-end,outer-microtask,inner-microtask"
    );
}
