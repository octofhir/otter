use otter_engine::EngineBuilder;

/// Helper to create an engine for testing
fn create_test_engine() -> otter_engine::Otter {
    EngineBuilder::new().build()
}

#[test]
fn test_arguments_length() {
    let mut engine = create_test_engine();
    let result = engine.eval_sync(
        r#"
        function foo(a, b) {
            return arguments.length;
        }
        foo(1, 2)
    "#,
    );
    match result {
        Ok(v) => assert_eq!(v.as_int32(), Some(2)),
        Err(e) => panic!("Failed: {:?}", e),
    }
}

#[test]
fn test_arguments_access() {
    let mut engine = create_test_engine();
    let result = engine.eval_sync(
        r#"
        function foo(a, b) {
            return arguments[0] + arguments[1];
        }
        foo(10, 20)
    "#,
    );
    match result {
        Ok(v) => assert_eq!(v.as_int32(), Some(30)),
        Err(e) => panic!("Failed: {:?}", e),
    }
}

#[test]
fn test_arguments_extra_args() {
    let mut engine = create_test_engine();
    let result = engine.eval_sync(
        r#"
        function foo(a) {
            return arguments.length + arguments[1];
        }
        foo(10, 20)
    "#,
    );
    match result {
        Ok(v) => assert_eq!(v.as_int32(), Some(22)),
        Err(e) => panic!("Failed: {:?}", e),
    }
}

#[test]
fn test_arguments_is_object() {
    let mut engine = create_test_engine();
    let result = engine.eval_sync(
        r#"
        function foo() {
            return typeof arguments;
        }
        foo()
    "#,
    );
    match result {
        Ok(v) => {
            println!("Value: {:?}", v);
            assert!(v.is_string());
            assert_eq!(v.as_string().unwrap().as_str(), "object");
        }
        Err(e) => panic!("Failed: {:?}", e),
    }
}

#[test]
fn test_arguments_instanceof_object() {
    let mut engine = create_test_engine();
    let result = engine.eval_sync(
        r#"
        function foo() {
            return arguments instanceof Object;
        }
        foo()
    "#,
    );
    match result {
        Ok(v) => assert_eq!(v.as_boolean(), Some(true)),
        Err(e) => panic!("Failed: {:?}", e),
    }
}
