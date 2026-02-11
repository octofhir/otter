use otter_engine::{EngineBuilder, NodeApiProfile};

#[test]
fn test_node_imports() {
    let mut otter = EngineBuilder::new().with_nodejs().build();

    let code = r#"
        import fs from 'node:fs';
        import path from 'path';

        // Verify imports are objects and have expected properties
        if (typeof fs.readFile !== 'function') throw new Error('fs.readFile is not a function');
        if (typeof path.join !== 'function') throw new Error('path.join is not a function');
        
        const result = path.join('foo', 'bar');
        if (result !== 'foo/bar') throw new Error('path.join failed: ' + result);
        
        'success'
    "#;

    match otter.eval_sync(code) {
        Ok(val) => {
            println!("Test result: {:?}", val);
            assert_eq!(val.as_string().map(|s| s.to_string()).unwrap(), "success");
        }
        Err(e) => panic!("Eval failed: {}", e),
    }
}

#[test]
fn test_node_safe_profile_allows_path() {
    let mut otter = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::SafeCore)
        .build();

    let code = r#"
        import path from 'node:path';
        path.join('foo', 'bar');
    "#;

    match otter.eval_sync(code) {
        Ok(val) => {
            assert_eq!(val.as_string().map(|s| s.to_string()).unwrap(), "foo/bar");
        }
        Err(e) => panic!("Eval failed: {}", e),
    }
}

#[test]
fn test_node_safe_profile_allows_bare_path() {
    let mut otter = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::SafeCore)
        .build();

    let code = r#"
        import path from 'path';
        path.join('foo', 'bar');
    "#;

    match otter.eval_sync(code) {
        Ok(val) => {
            assert_eq!(val.as_string().map(|s| s.to_string()).unwrap(), "foo/bar");
        }
        Err(e) => panic!("Eval failed: {}", e),
    }
}

#[test]
fn test_node_safe_profile_blocks_process() {
    let mut otter = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::SafeCore)
        .build();

    let code = r#"
        import process from 'node:process';
        process.version;
    "#;

    let result = otter.eval_sync(code);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("node:process"));
}

#[test]
fn test_node_safe_profile_blocks_bare_process() {
    let mut otter = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::SafeCore)
        .build();

    let code = r#"
        import process from 'process';
        process.version;
    "#;

    let result = otter.eval_sync(code);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("process"));
}
