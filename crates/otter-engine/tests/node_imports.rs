use otter_engine::EngineBuilder;
use std::sync::Arc;

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
