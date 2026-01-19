//! Integration tests for TypeScript support
//!
//! These tests verify the complete TypeScript pipeline: transpilation through
//! the SWC compiler and execution via JavaScriptCore.

use otter_runtime::{
    Engine, TsConfigJson, TypeScriptConfig, get_embedded_type, list_embedded_types,
    needs_transpilation, transpile_typescript,
};
use serde_json::json;
use std::io::Write;

// ============================================================================
// Transpiler Unit Tests
// ============================================================================

#[test]
fn test_transpile_basic() {
    let ts_code = r#"
        const x: number = 42;
        const y: string = "hello";
        x + y.length;
    "#;

    let result = transpile_typescript(ts_code).unwrap();
    assert!(!result.code.contains(": number"));
    assert!(!result.code.contains(": string"));
}

#[test]
fn test_transpile_interface() {
    let ts_code = r#"
        interface Person {
            name: string;
            age: number;
        }
        const p: Person = { name: "Alice", age: 30 };
        p.name;
    "#;

    let result = transpile_typescript(ts_code).unwrap();
    assert!(!result.code.contains("interface"));
}

#[test]
fn test_transpile_generics() {
    let ts_code = r#"
        function identity<T>(arg: T): T {
            return arg;
        }
        identity<number>(42);
    "#;

    let result = transpile_typescript(ts_code).unwrap();
    assert!(!result.code.contains("<T>"));
    assert!(!result.code.contains("<number>"));
}

#[test]
fn test_transpile_async_await() {
    let ts_code = r#"
        async function getData(): Promise<string> {
            return "data";
        }
    "#;

    let result = transpile_typescript(ts_code).unwrap();
    assert!(result.code.contains("async"));
}

#[test]
fn test_transpile_type_alias() {
    let ts_code = r#"
        type ID = string | number;
        type Status = "active" | "inactive";
        const id: ID = "abc";
        const status: Status = "active";
    "#;

    let result = transpile_typescript(ts_code).unwrap();
    assert!(!result.code.contains("type ID"));
    assert!(!result.code.contains("type Status"));
}

#[test]
fn test_transpile_enum() {
    let ts_code = r#"
        enum Color {
            Red,
            Green,
            Blue
        }
        const c: Color = Color.Red;
    "#;

    let result = transpile_typescript(ts_code).unwrap();
    // Enum should be transformed to JavaScript
    assert!(result.code.contains("Color"));
}

#[test]
fn test_transpile_class_with_visibility() {
    let ts_code = r#"
        class User {
            private id: number;
            public name: string;
            protected email: string;

            constructor(id: number, name: string, email: string) {
                this.id = id;
                this.name = name;
                this.email = email;
            }
        }
    "#;

    let result = transpile_typescript(ts_code).unwrap();
    // Visibility modifiers should be stripped
    assert!(!result.code.contains("private"));
    assert!(!result.code.contains("public"));
    assert!(!result.code.contains("protected"));
    assert!(result.code.contains("class User"));
}

#[test]
fn test_transpile_optional_chaining() {
    let ts_code = r#"
        interface Nested {
            a?: {
                b?: {
                    c?: number;
                }
            }
        }
        const obj: Nested = {};
        const val = obj?.a?.b?.c;
    "#;

    let result = transpile_typescript(ts_code).unwrap();
    // Should preserve optional chaining
    assert!(result.code.contains("?."));
}

#[test]
fn test_transpile_nullish_coalescing() {
    let ts_code = r#"
        const value: string | null = null;
        const result = value ?? "default";
    "#;

    let result = transpile_typescript(ts_code).unwrap();
    // Should preserve nullish coalescing
    assert!(result.code.contains("??"));
}

// ============================================================================
// needs_transpilation Tests
// ============================================================================

#[test]
fn test_needs_transpilation() {
    // TypeScript files
    assert!(needs_transpilation("file.ts"));
    assert!(needs_transpilation("file.tsx"));
    assert!(needs_transpilation("file.mts"));
    assert!(needs_transpilation("file.cts"));

    // JavaScript files (no transpilation needed)
    assert!(!needs_transpilation("file.js"));
    assert!(!needs_transpilation("file.mjs"));
    assert!(!needs_transpilation("file.jsx"));
    assert!(!needs_transpilation("file.cjs"));
}

// ============================================================================
// Embedded Types Tests
// ============================================================================

#[test]
fn test_embedded_types_available() {
    let types: Vec<_> = list_embedded_types().collect();
    assert!(!types.is_empty(), "No embedded types found");
}

#[test]
fn test_get_otter_types() {
    let otter_index = get_embedded_type("otter/index.d.ts");
    assert!(otter_index.is_some(), "otter/index.d.ts should be embedded");

    let content = otter_index.unwrap();
    // Otter types reference @types/node for Web/Node globals.
    assert!(
        content.contains("reference types=\"node\""),
        "Should reference @types/node"
    );

    // Spot-check that Otter module types are embedded.
    let sql_types = get_embedded_type("otter/sql.d.ts");
    assert!(sql_types.is_some(), "otter/sql.d.ts should be embedded");
    assert!(
        sql_types.unwrap().contains("declare module \"otter\""),
        "Should declare \"otter\" module"
    );
}

#[test]
fn test_get_node_path_types() {
    // Node types are referenced via `/// <reference types="node" />` and are
    // expected to be provided by the package manager (e.g. `@types/node`).
    assert!(get_embedded_type("node/path.d.ts").is_none());
}

#[test]
fn test_get_node_buffer_types() {
    assert!(get_embedded_type("node/buffer.d.ts").is_none());
}

#[test]
fn test_get_node_fs_types() {
    assert!(get_embedded_type("node/fs.d.ts").is_none());
}

#[test]
fn test_get_node_test_types() {
    assert!(get_embedded_type("node/test.d.ts").is_none());
}

// ============================================================================
// TypeScriptConfig Tests
// ============================================================================

#[test]
fn test_typescript_config_default() {
    let config = TypeScriptConfig::default();
    assert!(!config.check);
    assert!(config.tsx);
    assert!(config.decorators);
    assert!(!config.source_maps);
}

#[test]
fn test_typescript_config_builder() {
    let config = TypeScriptConfig::new().tsx(false).source_maps(true);

    assert!(!config.tsx);
    assert!(config.source_maps);
}

// ============================================================================
// Engine Integration Tests
// ============================================================================

#[tokio::test]
async fn test_execute_typescript_basic() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle
        .eval_typescript(
            r#"
            const x: number = 42;
            const y: number = 8;
            x + y
            "#,
        )
        .await
        .unwrap();

    assert_eq!(result, json!(50));
    engine.shutdown().await;
}

#[tokio::test]
async fn test_execute_typescript_interface() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle
        .eval_typescript(
            r#"
            interface Result {
                value: number;
                label: string;
            }
            const r: Result = { value: 42, label: "answer" };
            r.value
            "#,
        )
        .await
        .unwrap();

    assert_eq!(result, json!(42));
    engine.shutdown().await;
}

#[tokio::test]
async fn test_execute_typescript_generics() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle
        .eval_typescript(
            r#"
            function identity<T>(x: T): T {
                return x;
            }
            identity<string>("hello")
            "#,
        )
        .await
        .unwrap();

    assert_eq!(result, json!("hello"));
    engine.shutdown().await;
}

#[tokio::test]
async fn test_execute_typescript_array_generics() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle
        .eval_typescript(
            r#"
            function first<T>(arr: T[]): T | undefined {
                return arr[0];
            }
            first<number>([1, 2, 3])
            "#,
        )
        .await
        .unwrap();

    assert_eq!(result, json!(1));
    engine.shutdown().await;
}

#[tokio::test]
async fn test_execute_typescript_class() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle
        .eval_typescript(
            r#"
            class Calculator {
                private value: number;

                constructor(initial: number) {
                    this.value = initial;
                }

                add(n: number): Calculator {
                    this.value += n;
                    return this;
                }

                getResult(): number {
                    return this.value;
                }
            }

            new Calculator(10).add(5).add(3).getResult()
            "#,
        )
        .await
        .unwrap();

    assert_eq!(result, json!(18));
    engine.shutdown().await;
}

#[tokio::test]
async fn test_execute_typescript_with_source_url() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle
        .eval_typescript_with_source(
            r#"
            const message: string = "from typescript";
            message
            "#,
            "test.ts",
        )
        .await
        .unwrap();

    assert_eq!(result, json!("from typescript"));
    engine.shutdown().await;
}

#[tokio::test]
async fn test_typescript_syntax_error() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    // Invalid TypeScript syntax
    let result = handle
        .eval_typescript(
            r#"
            const x: = 42;  // Missing type
            "#,
        )
        .await;

    assert!(result.is_err());
    engine.shutdown().await;
}

#[tokio::test]
async fn test_typescript_complex_types() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle
        .eval_typescript(
            r#"
            type Maybe<T> = T | null | undefined;
            type User = {
                id: number;
                name: string;
                email?: string;
            };

            const user: Maybe<User> = {
                id: 1,
                name: "Alice"
            };

            user
            "#,
        )
        .await
        .unwrap();

    assert_eq!(result, json!({"id": 1, "name": "Alice"}));
    engine.shutdown().await;
}

#[tokio::test]
async fn test_typescript_mapped_types() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle
        .eval_typescript(
            r#"
            type Readonly<T> = {
                readonly [P in keyof T]: T[P];
            };

            interface Config {
                host: string;
                port: number;
            }

            const config: Readonly<Config> = {
                host: "localhost",
                port: 8080
            };

            config.port
            "#,
        )
        .await
        .unwrap();

    assert_eq!(result, json!(8080));
    engine.shutdown().await;
}

#[tokio::test]
async fn test_typescript_template_literals() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle
        .eval_typescript(
            r#"
            type Greeting = `Hello, ${string}!`;
            const greeting: Greeting = "Hello, World!";
            greeting
            "#,
        )
        .await
        .unwrap();

    assert_eq!(result, json!("Hello, World!"));
    engine.shutdown().await;
}

#[tokio::test]
async fn test_typescript_rest_params() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle
        .eval_typescript(
            r#"
            function sum(...numbers: number[]): number {
                return numbers.reduce((a, b) => a + b, 0);
            }
            sum(1, 2, 3, 4, 5)
            "#,
        )
        .await
        .unwrap();

    assert_eq!(result, json!(15));
    engine.shutdown().await;
}

#[tokio::test]
async fn test_typescript_destructuring() {
    let engine = Engine::new().unwrap();
    let handle = engine.handle();

    let result = handle
        .eval_typescript(
            r#"
            interface Point {
                x: number;
                y: number;
            }

            const point: Point = { x: 10, y: 20 };
            const { x, y }: Point = point;
            x + y
            "#,
        )
        .await
        .unwrap();

    assert_eq!(result, json!(30));
    engine.shutdown().await;
}

// ============================================================================
// tsconfig.json Tests
// ============================================================================

#[test]
fn test_tsconfig_parse_from_string() {
    let json = r#"
    {
        "compilerOptions": {
            "target": "ES2022",
            "module": "NodeNext",
            "strict": true,
            "sourceMap": true,
            "experimentalDecorators": true
        }
    }
    "#;

    let tsconfig = TsConfigJson::parse(json).unwrap();
    assert_eq!(tsconfig.compiler_options.target, Some("ES2022".to_string()));
    assert_eq!(tsconfig.compiler_options.strict, Some(true));
    assert_eq!(tsconfig.compiler_options.source_map, Some(true));
    assert_eq!(
        tsconfig.compiler_options.experimental_decorators,
        Some(true)
    );
}

#[test]
fn test_tsconfig_with_comments_and_trailing_commas() {
    let json = r#"
    {
        // TypeScript configuration
        "compilerOptions": {
            "target": "ES2020", // Target ES2020
            /* Enable strict mode
               for better type safety */
            "strict": true,
        },
    }
    "#;

    let tsconfig = TsConfigJson::parse(json).unwrap();
    assert_eq!(tsconfig.compiler_options.target, Some("ES2020".to_string()));
    assert_eq!(tsconfig.compiler_options.strict, Some(true));
}

#[test]
fn test_tsconfig_to_typescript_config() {
    let json = r#"
    {
        "compilerOptions": {
            "target": "ES2020",
            "strict": false,
            "sourceMap": true,
            "skipLibCheck": true,
            "experimentalDecorators": false,
            "jsx": "react"
        }
    }
    "#;

    let tsconfig = TsConfigJson::parse(json).unwrap();
    let config = tsconfig.to_typescript_config();

    assert_eq!(config.target, swc_ecma_ast::EsVersion::Es2020);
    assert!(!config.strict);
    assert!(config.source_maps);
    assert!(config.skip_lib_check);
    assert!(!config.decorators);
    assert!(config.tsx); // jsx != "none" means tsx is enabled
}

#[test]
fn test_typescript_config_from_tsconfig_file() {
    // Create a temporary tsconfig.json
    let temp_dir = std::env::temp_dir().join("otter_test_tsconfig");
    std::fs::create_dir_all(&temp_dir).unwrap();

    let tsconfig_path = temp_dir.join("tsconfig.json");
    let mut file = std::fs::File::create(&tsconfig_path).unwrap();
    file.write_all(
        br#"
        {
            "compilerOptions": {
                "target": "ES2021",
                "strict": true,
                "sourceMap": true
            }
        }
        "#,
    )
    .unwrap();

    // Load the config
    let config = TypeScriptConfig::from_tsconfig(&tsconfig_path).unwrap();

    assert_eq!(config.target, swc_ecma_ast::EsVersion::Es2021);
    assert!(config.strict);
    assert!(config.source_maps);
    assert_eq!(config.tsconfig, Some(tsconfig_path.clone()));

    // Cleanup
    std::fs::remove_dir_all(&temp_dir).ok();
}

#[test]
fn test_typescript_config_discover() {
    // Create a temp directory structure with tsconfig.json
    let temp_dir = std::env::temp_dir().join("otter_test_discover");
    let sub_dir = temp_dir.join("src").join("components");
    std::fs::create_dir_all(&sub_dir).unwrap();

    // Create tsconfig.json at root
    let tsconfig_path = temp_dir.join("tsconfig.json");
    let mut file = std::fs::File::create(&tsconfig_path).unwrap();
    file.write_all(
        br#"
        {
            "compilerOptions": {
                "target": "ES2022",
                "strict": true
            }
        }
        "#,
    )
    .unwrap();

    // Discover from sub directory should find root tsconfig
    let config = TypeScriptConfig::discover(&sub_dir).unwrap();

    assert_eq!(config.target, swc_ecma_ast::EsVersion::Es2022);
    assert!(config.strict);

    // Cleanup
    std::fs::remove_dir_all(&temp_dir).ok();
}

#[test]
fn test_tsconfig_paths_and_includes() {
    let json = r#"
    {
        "compilerOptions": {
            "target": "ES2022",
            "baseUrl": ".",
            "paths": {
                "@/*": ["./src/*"],
                "@components/*": ["./src/components/*"]
            }
        },
        "include": ["src/**/*"],
        "exclude": ["node_modules", "dist"]
    }
    "#;

    let tsconfig = TsConfigJson::parse(json).unwrap();

    assert!(tsconfig.compiler_options.paths.is_some());
    let paths = tsconfig.compiler_options.paths.unwrap();
    assert_eq!(paths.get("@/*"), Some(&vec!["./src/*".to_string()]));

    assert_eq!(tsconfig.include, vec!["src/**/*"]);
    assert_eq!(tsconfig.exclude, vec!["node_modules", "dist"]);
}
