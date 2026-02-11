use otter_engine::{EngineBuilder, Otter};

fn assert_ok(otter: &mut Otter, code: &str) {
    let value = otter
        .eval_sync(code)
        .unwrap_or_else(|e| panic!("Eval failed: {e}"));
    let out = value.as_string().map(|s| s.to_string()).unwrap_or_default();
    assert_eq!(out, "ok");
}

#[test]
fn test_abort_controller_and_signal_basics() {
    let mut otter = EngineBuilder::new().build();
    assert_ok(
        &mut otter,
        "const controller = new AbortController();\n\
         if (controller.signal.aborted !== false) throw new Error('initial aborted');\n\
         if (controller.signal.reason !== undefined) throw new Error('initial reason');\n\
         let onabortCalled = false;\n\
         controller.signal.onabort = (event) => {\n\
             onabortCalled = event.type === 'abort' && event.target === controller.signal;\n\
         };\n\
         const reason = new Error('boom');\n\
         controller.abort(reason);\n\
         if (controller.signal.aborted !== true) throw new Error('aborted');\n\
         if (controller.signal.reason !== reason) throw new Error('reason identity');\n\
         if (!onabortCalled) throw new Error('onabort callback');\n\
         let threwSameReason = false;\n\
         try { controller.signal.throwIfAborted(); } catch (e) { threwSameReason = e === reason; }\n\
         if (!threwSameReason) throw new Error('throwIfAborted reason');\n\
         'ok';",
    );
}

#[test]
fn test_abort_signal_timeout_uses_timers_when_available() {
    let mut otter = EngineBuilder::new().build();

    assert_ok(
        &mut otter,
        "let called = false;\n\
         let delaySeen = -1;\n\
         const originalSetTimeout = setTimeout;\n\
         globalThis.setTimeout = (cb, ms, ...rest) => {\n\
             called = true;\n\
             delaySeen = ms;\n\
             return originalSetTimeout(cb, 0, ...rest);\n\
         };\n\
         const signal = AbortSignal.timeout(7);\n\
         if (typeof signal !== 'object') throw new Error('signal object');\n\
         if (!called) throw new Error('setTimeout not used');\n\
         if (delaySeen !== 7) throw new Error('delay mismatch: ' + delaySeen);\n\
         globalThis.setTimeout = originalSetTimeout;\n\
         'ok';",
    );
}

#[test]
fn test_timer_globals_available_without_node_profile() {
    let mut otter = EngineBuilder::new().build();

    assert_ok(
        &mut otter,
        "for (const name of ['setTimeout', 'clearTimeout', 'setInterval', 'clearInterval', 'setImmediate', 'clearImmediate', 'queueMicrotask']) {\n\
             if (typeof globalThis[name] !== 'function') throw new Error('missing timer fn ' + name);\n\
         }\n\
         const timeoutId = setTimeout(() => {}, 1);\n\
         const intervalId = setInterval(() => {}, 1);\n\
         const immediateId = setImmediate(() => {});\n\
         if (typeof timeoutId !== 'number') throw new Error('setTimeout id');\n\
         if (typeof intervalId !== 'number') throw new Error('setInterval id');\n\
         if (typeof immediateId !== 'number') throw new Error('setImmediate id');\n\
         clearTimeout(timeoutId);\n\
         clearInterval(intervalId);\n\
         clearImmediate(immediateId);\n\
         'ok';",
    );
}
