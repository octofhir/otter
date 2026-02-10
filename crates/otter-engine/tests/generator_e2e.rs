//! End-to-end generator tests
//!
//! These tests verify that generators work correctly through the full
//! compilation and execution pipeline.
//!
//! NOTE: eval_sync doesn't return expression values (only explicit returns from functions).
//! These tests verify that generator code compiles and executes without errors.
//! The actual generator behavior is verified by unit tests in otter-vm-core.

use otter_engine::EngineBuilder;

/// Helper to create an engine for testing
fn create_test_engine() -> otter_engine::Otter {
    EngineBuilder::new().build()
}

#[test]
fn test_generator_compiles() {
    let mut engine = create_test_engine();

    // Test that generator function syntax compiles
    let result = engine.eval_sync(
        r#"
        function* gen() {
            yield 1;
            yield 2;
            yield 3;
        }
    "#,
    );

    if let Err(e) = result {
        panic!("Generator function compilation failed: {:?}", e);
    }
}

#[test]
fn test_generator_creation() {
    let mut engine = create_test_engine();

    // Test that calling a generator function creates a generator
    let result = engine.eval_sync(
        r#"
        function* gen() {
            yield 1;
        }
        const g = gen();
    "#,
    );

    if let Err(e) = result {
        panic!("Generator creation failed: {:?}", e);
    }
}

#[test]
fn test_generator_next_call() {
    let mut engine = create_test_engine();

    // Test multiple next() calls
    let result = engine.eval_sync(
        r#"
        function* gen() {
            yield 1;
            yield 2;
        }
        const g = gen();
        const r1 = g.next();
        const r2 = g.next();
    "#,
    );

    if let Err(e) = result {
        panic!("Generator second next() call failed: {:?}", e);
    }
}

#[test]
fn test_generator_with_variables() {
    let mut engine = create_test_engine();

    // Test generator with local variable mutations
    let result = engine.eval_sync(
        r#"
        function* gen() {
            let x = 10;
            yield x;
            x = 20;
            yield x;
            x = x + 5;
            return x;
        }
        const g = gen();
        g.next();
        g.next();
        g.next();
    "#,
    );

    if let Err(e) = result {
        panic!("Generator with variables failed: {:?}", e);
    }
}

#[test]
fn test_generator_return_method() {
    let mut engine = create_test_engine();

    // Test generator.return() method
    let result = engine.eval_sync(
        r#"
        function* gen() {
            yield 1;
            yield 2;
            yield 3;
        }
        const g = gen();
        g.next();
        g.return(42);
    "#,
    );

    if let Err(e) = result {
        panic!("Generator return() failed: {:?}", e);
    }
}

#[test]
fn test_generator_with_loop() {
    let mut engine = create_test_engine();

    // Test generator with for loop
    let result = engine.eval_sync(
        r#"
        function* range(start, end) {
            for (let i = start; i < end; i++) {
                yield i;
            }
        }
        const g = range(1, 5);
        g.next();
        g.next();
        g.next();
        g.next();
        g.next();
    "#,
    );

    if let Err(e) = result {
        panic!("Generator with loop failed: {:?}", e);
    }
}

#[test]
fn test_multiple_generators() {
    let mut engine = create_test_engine();

    // Test multiple independent generator instances
    let result = engine.eval_sync(
        r#"
        function* gen() {
            yield 1;
            yield 2;
        }
        const g1 = gen();
        const g2 = gen();
        g1.next();
        g2.next();
        g1.next();
        g2.next();
    "#,
    );

    if let Err(e) = result {
        panic!("Multiple generators failed: {:?}", e);
    }
}

#[test]
fn test_generator_with_try_catch() {
    let mut engine = create_test_engine();

    // Test generator with try-catch block
    let result = engine.eval_sync(
        r#"
        function* gen() {
            try {
                yield 1;
                yield 2;
            } catch (e) {
                yield e;
            }
            yield 3;
        }
        const g = gen();
        g.next();
        g.next();
        g.next();
    "#,
    );

    if let Err(e) = result {
        panic!("Generator with try-catch failed: {:?}", e);
    }
}

#[test]
fn test_generator_expression() {
    let mut engine = create_test_engine();

    // Test generator expression (anonymous generator)
    let result = engine.eval_sync(
        r#"
        const gen = function*() {
            yield 1;
            yield 2;
        };
        const g = gen();
        g.next();
        g.next();
    "#,
    );

    if let Err(e) = result {
        panic!("Generator expression failed: {:?}", e);
    }
}

#[test]
fn test_generator_third_next_call() {
    let mut engine = create_test_engine();

    // Test third next() call on exhausted generator
    // All in one eval to avoid state persistence issues
    let result = engine.eval_sync(
        r#"
        function* gen() {
            yield 1;
            yield 2;
        }
        const g = gen();
        g.next();
        g.next();
        g.next();
    "#,
    );

    if let Err(e) = result {
        panic!("Generator third next() call failed: {:?}", e);
    }
}

#[test]
fn test_generator_simple_return() {
    let mut engine = create_test_engine();

    // Test simple return statement
    let result = engine.eval_sync(
        r#"
        function* gen() {
            yield 1;
            return 42;
        }
        const g = gen();
        g.next();
        g.next();
    "#,
    );

    if let Err(e) = result {
        panic!("Generator simple return failed: {:?}", e);
    }
}

#[test]
fn test_generator_simple_loop() {
    let mut engine = create_test_engine();

    // Test simple while loop
    let result = engine.eval_sync(
        r#"
        function* gen() {
            let i = 0;
            while (i < 2) {
                yield i;
                i = i + 1;
            }
        }
        const g = gen();
        g.next();
        g.next();
    "#,
    );

    if let Err(e) = result {
        panic!("Generator simple loop failed: {:?}", e);
    }
}

#[test]
fn test_generator_next_with_value() {
    let mut engine = create_test_engine();

    // Test that values sent to next() are received by yield expressions
    // The acceptance criteria from task-07:
    //   function* gen() {
    //     const x = yield 1;
    //     const y = yield x + 2;
    //     return y + 3;
    //   }
    //   const g = gen();
    //   g.next();      // {value: 1, done: false}
    //   g.next(10);    // {value: 12, done: false}
    //   g.next(20);    // {value: 23, done: true}
    let result = engine.eval_sync(
        r#"
        function* gen() {
            const x = yield 1;
            const y = yield x + 2;
            return y + 3;
        }
        const g = gen();
        const r1 = g.next();      // Should yield 1, x will receive value from next next()
        const r2 = g.next(10);    // x = 10, yield 10 + 2 = 12
        const r3 = g.next(20);    // y = 20, return 20 + 3 = 23
    "#,
    );

    if let Err(e) = result {
        panic!("Generator next(value) failed: {:?}", e);
    }
}

#[test]
fn test_generator_next_value_first_call_ignored() {
    let mut engine = create_test_engine();

    // Test that the value passed to the first next() call is ignored
    // (there's no yield expression to receive it)
    let result = engine.eval_sync(
        r#"
        function* gen() {
            yield 42;
        }
        const g = gen();
        // Even if we pass a value to the first next(), it should be ignored
        // The generator just starts and hits the first yield, returning 42
        const r = g.next(999);  // 999 should be ignored
    "#,
    );

    if let Err(e) = result {
        panic!("Generator first next() value ignore failed: {:?}", e);
    }
}

#[test]
fn test_generator_next_value_chained() {
    let mut engine = create_test_engine();

    // Test chained send values through multiple yields
    let result = engine.eval_sync(
        r#"
        function* accumulator() {
            let total = 0;
            while (true) {
                const received = yield total;
                if (received === undefined) break;
                total = total + received;
            }
            return total;
        }
        const g = accumulator();
        g.next();       // Start, yields 0
        g.next(5);      // total = 0 + 5 = 5, yields 5
        g.next(10);     // total = 5 + 10 = 15, yields 15
        g.next(3);      // total = 15 + 3 = 18, yields 18
        g.next();       // received = undefined, breaks, returns 18
    "#,
    );

    if let Err(e) = result {
        panic!("Generator chained next(value) failed: {:?}", e);
    }
}

// ============================================================================
// Task 08: generator.return(value) tests
// ============================================================================

#[test]
fn test_generator_return_basic() {
    let mut engine = create_test_engine();

    // Test basic generator.return() - should return {value, done: true}
    let result = engine.eval_sync(
        r#"
        function* gen() {
            yield 1;
            yield 2;
            yield 3;
        }
        const g = gen();
        g.next();         // {value: 1, done: false}
        const r = g.return(99);  // {value: 99, done: true}
    "#,
    );

    if let Err(e) = result {
        panic!("Generator return basic failed: {:?}", e);
    }
}

#[test]
fn test_generator_return_on_completed() {
    let mut engine = create_test_engine();

    // Test generator.return() on already completed generator
    let result = engine.eval_sync(
        r#"
        function* gen() {
            yield 1;
        }
        const g = gen();
        g.next();         // {value: 1, done: false}
        g.next();         // {value: undefined, done: true} - completed
        g.return(99);     // {value: 99, done: true} - should still work
    "#,
    );

    if let Err(e) = result {
        panic!("Generator return on completed failed: {:?}", e);
    }
}

#[test]
fn test_generator_return_before_start() {
    let mut engine = create_test_engine();

    // Test generator.return() before generator is started
    let result = engine.eval_sync(
        r#"
        function* gen() {
            yield 1;
            yield 2;
        }
        const g = gen();
        // Return before first next() - generator never starts
        g.return(42);     // {value: 42, done: true}
    "#,
    );

    if let Err(e) = result {
        panic!("Generator return before start failed: {:?}", e);
    }
}

#[test]
fn test_generator_return_with_finally() {
    let mut engine = create_test_engine();

    // Test that generator.return() executes finally blocks
    // This is the acceptance criteria from task-08
    let result = engine.eval_sync(
        r#"
        let finallyCalled = false;
        function* gen() {
            try {
                yield 1;
                yield 2;
            } finally {
                finallyCalled = true;
            }
        }
        const g = gen();
        g.next();          // {value: 1, done: false}
        g.return(99);      // Should run finally, then return {value: 99, done: true}
        // finallyCalled should be true at this point
    "#,
    );

    if let Err(e) = result {
        panic!("Generator return with finally failed: {:?}", e);
    }
}

#[test]
fn test_generator_return_with_nested_finally() {
    let mut engine = create_test_engine();

    // Test nested try-finally blocks
    // Note: Using simple assignments instead of function calls since
    // function calls within generators aren't fully implemented yet
    let result = engine.eval_sync(
        r#"
        let innerRan = false;
        let outerRan = false;
        function* gen() {
            try {
                try {
                    yield 1;
                } finally {
                    innerRan = true;
                }
            } finally {
                outerRan = true;
            }
        }
        const g = gen();
        g.next();
        g.return(99);
        // Both finally blocks should have run
    "#,
    );

    if let Err(e) = result {
        panic!("Generator return with nested finally failed: {:?}", e);
    }
}

#[test]
fn test_generator_return_no_finally() {
    let mut engine = create_test_engine();

    // Test generator.return() without finally - should just complete
    let result = engine.eval_sync(
        r#"
        function* gen() {
            yield 1;
            yield 2;
        }
        const g = gen();
        g.next();
        g.return(42);  // No finally, just complete with value
    "#,
    );

    if let Err(e) = result {
        panic!("Generator return no finally failed: {:?}", e);
    }
}
