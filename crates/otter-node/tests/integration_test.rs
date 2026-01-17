//! Integration tests for Node.js modules.
//!
//! These tests verify that the Node module extensions work correctly when executed
//! via the Otter runtime engine.
//!
//! Native functions are registered with `path_` prefix (e.g., `path_join`, `path_dirname`).
//! The JS wrapper in path.js provides the proper Node.js-compatible API via require('path').

use otter_engine::CapabilitiesBuilder;
use otter_node::{
    create_buffer_extension, create_crypto_extension, create_fs_extension, create_path_extension,
    create_test_extension,
};
use otter_runtime::{Engine, transform_module, wrap_module};
use tempfile::TempDir;
use std::collections::HashMap;

/// Test path module via JavaScript execution.
mod path_tests {
    use super::*;

    #[tokio::test]
    async fn test_path_join() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_path_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        // Test join operation (native function with path_ prefix, takes array)
        let result = handle
            .eval(r#"path_join(["foo", "bar", "baz.txt"])"#)
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!("foo/bar/baz.txt"));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_path_dirname() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_path_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle.eval(r#"path_dirname("/foo/bar/baz.txt")"#).await.unwrap();
        assert_eq!(result, serde_json::json!("/foo/bar"));

        let result = handle.eval(r#"path_dirname("baz.txt")"#).await.unwrap();
        assert_eq!(result, serde_json::json!("."));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_path_basename() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_path_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle
            .eval(r#"path_basename("/foo/bar/baz.txt", null)"#)
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!("baz.txt"));

        let result = handle
            .eval(r#"path_basename("/foo/bar/baz.txt", ".txt")"#)
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!("baz"));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_path_extname() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_path_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle.eval(r#"path_extname("file.txt")"#).await.unwrap();
        assert_eq!(result, serde_json::json!(".txt"));

        let result = handle.eval(r#"path_extname("file")"#).await.unwrap();
        assert_eq!(result, serde_json::json!(""));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_path_is_absolute() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_path_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle.eval(r#"path_is_absolute("/foo/bar")"#).await.unwrap();
        assert_eq!(result, serde_json::json!(true));

        let result = handle.eval(r#"path_is_absolute("foo/bar")"#).await.unwrap();
        assert_eq!(result, serde_json::json!(false));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_path_normalize() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_path_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle
            .eval(r#"path_normalize("/foo/bar/../baz")"#)
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!("/foo/baz"));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_path_parse() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_path_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle
            .eval(r#"JSON.stringify(path_parse("/home/user/file.txt"))"#)
            .await
            .unwrap();

        let parsed: serde_json::Value = serde_json::from_str(result.as_str().unwrap()).unwrap();
        assert_eq!(parsed["root"], "/");
        assert_eq!(parsed["dir"], "/home/user");
        assert_eq!(parsed["base"], "file.txt");
        assert_eq!(parsed["ext"], ".txt");
        assert_eq!(parsed["name"], "file");

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_path_sep() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_path_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle.eval(r#"path_sep()"#).await.unwrap();
        // Should be "/" on Unix, "\" on Windows
        assert!(result == serde_json::json!("/") || result == serde_json::json!("\\"));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_path_relative() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_path_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle
            .eval(r#"path_relative("/foo/bar", "/foo/baz")"#)
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!("../baz"));

        engine.shutdown().await;
    }
}

/// Test buffer module via JavaScript execution.
mod buffer_tests {
    use super::*;

    #[tokio::test]
    async fn test_buffer_alloc() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_buffer_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle.eval(r#"alloc(5, 0)"#).await.unwrap();
        let data = result.get("data").unwrap().as_array().unwrap();
        assert_eq!(data.len(), 5);
        assert!(data.iter().all(|v| v.as_u64() == Some(0)));

        let result = handle.eval(r#"alloc(3, 42)"#).await.unwrap();
        let data = result.get("data").unwrap().as_array().unwrap();
        assert_eq!(data.len(), 3);
        assert!(data.iter().all(|v| v.as_u64() == Some(42)));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_buffer_from_string() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_buffer_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle.eval(r#"from("hello", "utf8")"#).await.unwrap();
        let data = result.get("data").unwrap().as_array().unwrap();
        let bytes: Vec<u8> = data.iter().map(|v| v.as_u64().unwrap() as u8).collect();
        assert_eq!(bytes, b"hello");

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_buffer_from_base64() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_buffer_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        // "aGVsbG8=" is "hello" in base64
        let result = handle.eval(r#"from("aGVsbG8=", "base64")"#).await.unwrap();
        let data = result.get("data").unwrap().as_array().unwrap();
        let bytes: Vec<u8> = data.iter().map(|v| v.as_u64().unwrap() as u8).collect();
        assert_eq!(bytes, b"hello");

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_buffer_from_hex() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_buffer_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        // "68656c6c6f" is "hello" in hex
        let result = handle.eval(r#"from("68656c6c6f", "hex")"#).await.unwrap();
        let data = result.get("data").unwrap().as_array().unwrap();
        let bytes: Vec<u8> = data.iter().map(|v| v.as_u64().unwrap() as u8).collect();
        assert_eq!(bytes, b"hello");

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_buffer_to_string() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_buffer_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        // Create buffer and convert to string
        let result = handle
            .eval(
                r#"
                const buf = from("hello", "utf8");
                toString(buf, "utf8", 0, 5)
            "#,
            )
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!("hello"));

        // Test base64 encoding
        let result = handle
            .eval(
                r#"
                const buf2 = from("hello", "utf8");
                toString(buf2, "base64", 0, 5)
            "#,
            )
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!("aGVsbG8="));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_buffer_concat() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_buffer_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle
            .eval(
                r#"
                const buf1 = from("hello", "utf8");
                const buf2 = from(" ", "utf8");
                const buf3 = from("world", "utf8");
                concat([buf1, buf2, buf3])
            "#,
            )
            .await
            .unwrap();

        let data = result.get("data").unwrap().as_array().unwrap();
        let bytes: Vec<u8> = data.iter().map(|v| v.as_u64().unwrap() as u8).collect();
        assert_eq!(bytes, b"hello world");

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_buffer_slice() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_buffer_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle
            .eval(
                r#"
                const buf = from("hello world", "utf8");
                slice(buf, 0, 5)
            "#,
            )
            .await
            .unwrap();

        let data = result.get("data").unwrap().as_array().unwrap();
        let bytes: Vec<u8> = data.iter().map(|v| v.as_u64().unwrap() as u8).collect();
        assert_eq!(bytes, b"hello");

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_buffer_equals() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_buffer_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle
            .eval(
                r#"
                const buf1 = from("hello", "utf8");
                const buf2 = from("hello", "utf8");
                equals(buf1, buf2)
            "#,
            )
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!(true));

        let result = handle
            .eval(
                r#"
                const buf3 = from("hello", "utf8");
                const buf4 = from("world", "utf8");
                equals(buf3, buf4)
            "#,
            )
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!(false));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_buffer_compare() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_buffer_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle
            .eval(
                r#"
                const buf1 = from("abc", "utf8");
                const buf2 = from("abc", "utf8");
                compare(buf1, buf2)
            "#,
            )
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!(0));

        let result = handle
            .eval(
                r#"
                const buf3 = from("abc", "utf8");
                const buf4 = from("abd", "utf8");
                compare(buf3, buf4)
            "#,
            )
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!(-1));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_buffer_is_buffer() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_buffer_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle
            .eval(
                r#"
                const buf = from("hello", "utf8");
                isBuffer(buf)
            "#,
            )
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!(true));

        let result = handle.eval(r#"isBuffer("not a buffer")"#).await.unwrap();
        assert_eq!(result, serde_json::json!(false));

        engine.shutdown().await;
    }
}

/// Test fs module via JavaScript execution.
/// Uses sync methods (readFileSync, writeFileSync, etc.) for simpler testing.
mod fs_tests {
    use super::*;

    fn canonical_temp_path(temp: &TempDir) -> std::path::PathBuf {
        temp.path()
            .canonicalize()
            .unwrap_or_else(|_| temp.path().to_path_buf())
    }

    #[tokio::test]
    async fn test_fs_write_and_read_file_sync() {
        let temp = TempDir::new().unwrap();
        let temp_path = canonical_temp_path(&temp);
        let file_path = temp_path.join("test.txt");

        let caps = CapabilitiesBuilder::new()
            .allow_read(vec![temp_path.clone()])
            .allow_write(vec![temp_path.clone()])
            .build();

        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_fs_extension(caps))
            .build()
            .unwrap();

        let handle = engine.handle();

        // Write file using sync method
        let script = format!(
            r#"writeFileSync("{}", "hello world")"#,
            file_path.to_string_lossy().replace('\\', "\\\\")
        );
        handle.eval(&script).await.unwrap();

        // Read file as string using sync method
        let script = format!(
            r#"readFileSync("{}", "utf8")"#,
            file_path.to_string_lossy().replace('\\', "\\\\")
        );
        let result = handle.eval(&script).await.unwrap();
        assert_eq!(result, serde_json::json!("hello world"));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_fs_readdir_sync() {
        let temp = TempDir::new().unwrap();
        let temp_path = canonical_temp_path(&temp);

        // Create test files
        std::fs::write(temp_path.join("file1.txt"), "content1").unwrap();
        std::fs::write(temp_path.join("file2.txt"), "content2").unwrap();

        let caps = CapabilitiesBuilder::new()
            .allow_read(vec![temp_path.clone()])
            .build();

        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_fs_extension(caps))
            .build()
            .unwrap();

        let handle = engine.handle();

        let script = format!(
            r#"readdirSync("{}")"#,
            temp_path.to_string_lossy().replace('\\', "\\\\")
        );
        let result = handle.eval(&script).await.unwrap();

        let entries = result.as_array().unwrap();
        let names: Vec<&str> = entries.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"file1.txt"));
        assert!(names.contains(&"file2.txt"));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_fs_stat_sync() {
        let temp = TempDir::new().unwrap();
        let temp_path = canonical_temp_path(&temp);
        let file_path = temp_path.join("test.txt");

        std::fs::write(&file_path, "hello").unwrap();

        let caps = CapabilitiesBuilder::new()
            .allow_read(vec![temp_path.clone()])
            .build();

        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_fs_extension(caps))
            .build()
            .unwrap();

        let handle = engine.handle();

        let script = format!(
            r#"statSync("{}")"#,
            file_path.to_string_lossy().replace('\\', "\\\\")
        );
        let result = handle.eval(&script).await.unwrap();

        assert_eq!(result["isFile"], serde_json::json!(true));
        assert_eq!(result["isDirectory"], serde_json::json!(false));
        assert_eq!(result["size"], serde_json::json!(5));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_fs_mkdir_and_rm_sync() {
        let temp = TempDir::new().unwrap();
        let temp_path = canonical_temp_path(&temp);
        let dir_path = temp_path.join("subdir");

        let caps = CapabilitiesBuilder::new()
            .allow_read(vec![temp_path.clone()])
            .allow_write(vec![temp_path.clone()])
            .build();

        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_fs_extension(caps))
            .build()
            .unwrap();

        let handle = engine.handle();

        // Create directory
        let script = format!(
            r#"mkdirSync("{}")"#,
            dir_path.to_string_lossy().replace('\\', "\\\\")
        );
        handle.eval(&script).await.unwrap();
        assert!(dir_path.exists());

        // Remove directory
        let script = format!(
            r#"rmSync("{}")"#,
            dir_path.to_string_lossy().replace('\\', "\\\\")
        );
        handle.eval(&script).await.unwrap();
        assert!(!dir_path.exists());

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_fs_exists_sync() {
        let temp = TempDir::new().unwrap();
        let temp_path = canonical_temp_path(&temp);
        let file_path = temp_path.join("test.txt");

        let caps = CapabilitiesBuilder::new()
            .allow_read(vec![temp_path.clone()])
            .build();

        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_fs_extension(caps))
            .build()
            .unwrap();

        let handle = engine.handle();

        // File doesn't exist yet
        let script = format!(
            r#"existsSync("{}")"#,
            file_path.to_string_lossy().replace('\\', "\\\\")
        );
        let result = handle.eval(&script).await.unwrap();
        assert_eq!(result, serde_json::json!(false));

        // Create file
        std::fs::write(&file_path, "hello").unwrap();

        // Now file exists
        let result = handle.eval(&script).await.unwrap();
        assert_eq!(result, serde_json::json!(true));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_fs_permission_denied_sync() {
        let temp = TempDir::new().unwrap();
        let temp_path = canonical_temp_path(&temp);
        let file_path = temp_path.join("test.txt");

        // No permissions granted
        let caps = otter_engine::Capabilities::none();

        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_fs_extension(caps))
            .build()
            .unwrap();

        let handle = engine.handle();

        let script = format!(
            r#"readFileSync("{}", "utf8")"#,
            file_path.to_string_lossy().replace('\\', "\\\\")
        );
        let result = handle.eval(&script).await;

        // Should fail with permission denied
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Permission denied") || err.contains("permission"),
            "Expected permission error, got: {}",
            err
        );

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_fs_copy_file_sync() {
        let temp = TempDir::new().unwrap();
        let temp_path = canonical_temp_path(&temp);
        let src = temp_path.join("src.txt");
        let dest = temp_path.join("dest.txt");

        std::fs::write(&src, "hello world").unwrap();

        let caps = CapabilitiesBuilder::new()
            .allow_read(vec![temp_path.clone()])
            .allow_write(vec![temp_path.clone()])
            .build();

        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_fs_extension(caps))
            .build()
            .unwrap();

        let handle = engine.handle();

        let script = format!(
            r#"copyFileSync("{}", "{}")"#,
            src.to_string_lossy().replace('\\', "\\\\"),
            dest.to_string_lossy().replace('\\', "\\\\")
        );
        let result = handle.eval(&script).await.unwrap();
        assert_eq!(result, serde_json::json!(11)); // 11 bytes copied

        assert!(dest.exists());
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "hello world");

        engine.shutdown().await;
    }
}

/// Verify that node:* builtins can be imported from bundled modules.
mod node_builtin_import_tests {
    use super::*;

    #[tokio::test]
    async fn test_import_node_path() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_path_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let mut deps = HashMap::new();
        deps.insert("node:path".to_string(), "node:path".to_string());

        let source = r#"
            import { join } from 'node:path';
            export const out = join('a', 'b', 'c.txt');
        "#;

        let transformed = transform_module(source, "file:///test/main.js", &deps);
        let wrapped = wrap_module("file:///test/main.js", &transformed);
        let bundle = format!("globalThis.__otter_modules = globalThis.__otter_modules || {{}};\n{wrapped}");

        handle.eval(&bundle).await.unwrap();
        let result = handle
            .eval(r#"__otter_modules["file:///test/main.js"].out"#)
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!("a/b/c.txt"));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_import_node_fs_shape() {
        let temp = TempDir::new().unwrap();
        let temp_path = temp.path().to_path_buf();

        let caps = CapabilitiesBuilder::new()
            .allow_read(vec![temp_path.clone()])
            .allow_write(vec![temp_path.clone()])
            .build();

        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_fs_extension(caps))
            .build()
            .unwrap();

        let handle = engine.handle();

        let mut deps = HashMap::new();
        deps.insert("node:fs".to_string(), "node:fs".to_string());

        let source = r#"
            import fs from 'node:fs';
            export const ok =
                typeof fs.readFileSync === 'function' &&
                typeof fs.writeFileSync === 'function' &&
                fs.promises && typeof fs.promises.readFile === 'function';
        "#;

        let transformed = transform_module(source, "file:///test/main.js", &deps);
        let wrapped = wrap_module("file:///test/main.js", &transformed);
        let bundle = format!("globalThis.__otter_modules = globalThis.__otter_modules || {{}};\n{wrapped}");

        handle.eval(&bundle).await.unwrap();
        let result = handle
            .eval(r#"__otter_modules["file:///test/main.js"].ok"#)
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!(true));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_import_node_buffer() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_buffer_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let mut deps = HashMap::new();
        deps.insert("node:buffer".to_string(), "node:buffer".to_string());

        let source = r#"
            import { Buffer } from 'node:buffer';
            export const out = Buffer.from('hello', 'utf8').toString('utf8', 0, 5);
        "#;

        let transformed = transform_module(source, "file:///test/main.js", &deps);
        let wrapped = wrap_module("file:///test/main.js", &transformed);
        let bundle = format!("globalThis.__otter_modules = globalThis.__otter_modules || {{}};\n{wrapped}");

        handle.eval(&bundle).await.unwrap();
        let result = handle
            .eval(r#"__otter_modules["file:///test/main.js"].out"#)
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!("hello"));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_import_node_crypto_shape() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_crypto_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let mut deps = HashMap::new();
        deps.insert("node:crypto".to_string(), "node:crypto".to_string());

        let source = r#"
            import crypto from 'node:crypto';
            export const ok =
                typeof crypto.randomUUID === 'function' &&
                typeof crypto.createHash === 'function' &&
                typeof crypto.createHmac === 'function';
        "#;

        let transformed = transform_module(source, "file:///test/main.js", &deps);
        let wrapped = wrap_module("file:///test/main.js", &transformed);
        let bundle = format!("globalThis.__otter_modules = globalThis.__otter_modules || {{}};\n{wrapped}");

        handle.eval(&bundle).await.unwrap();
        let result = handle
            .eval(r#"__otter_modules["file:///test/main.js"].ok"#)
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!(true));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_import_node_test_shape() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let mut deps = HashMap::new();
        deps.insert("node:test".to_string(), "node:test".to_string());

        let source = r#"
            import test from 'node:test';
            export const ok =
                typeof test.describe === 'function' &&
                typeof test.it === 'function' &&
                typeof test.run === 'function' &&
                test.assert && typeof test.assert.equal === 'function';
        "#;

        let transformed = transform_module(source, "file:///test/main.js", &deps);
        let wrapped = wrap_module("file:///test/main.js", &transformed);
        let bundle = format!("globalThis.__otter_modules = globalThis.__otter_modules || {{}};\n{wrapped}");

        handle.eval(&bundle).await.unwrap();
        let result = handle
            .eval(r#"__otter_modules["file:///test/main.js"].ok"#)
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!(true));

        engine.shutdown().await;
    }
}

/// Test test module via JavaScript execution.
mod test_tests {
    use super::*;

    #[tokio::test]
    async fn test_describe_and_end_describe() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        // Start and end a describe block using internal ops
        handle.eval(r#"__startSuite("Math")"#).await.unwrap();
        handle.eval(r#"__endSuite()"#).await.unwrap();

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_record_result() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        // Record a passing test
        handle
            .eval(r#"__recordResult("test1", true, 10, null)"#)
            .await
            .unwrap();

        // Record a failing test
        handle
            .eval(r#"__recordResult("test2", false, 5, "expected 1, got 2")"#)
            .await
            .unwrap();

        // Get summary
        let result = handle.eval(r#"__getSummary()"#).await.unwrap();

        assert_eq!(result["passed"], serde_json::json!(1));
        assert_eq!(result["failed"], serde_json::json!(1));
        assert_eq!(result["total"], serde_json::json!(2));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_skip() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        handle.eval(r#"__skipTest("skipped test")"#).await.unwrap();

        let result = handle.eval(r#"__getSummary()"#).await.unwrap();

        assert_eq!(result["skipped"], serde_json::json!(1));
        assert_eq!(result["total"], serde_json::json!(1));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_nested_suites() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        // Create nested suites using internal ops
        handle.eval(r#"__startSuite("Math")"#).await.unwrap();
        handle.eval(r#"__startSuite("addition")"#).await.unwrap();
        handle
            .eval(r#"__recordResult("adds numbers", true, 5, null)"#)
            .await
            .unwrap();
        handle.eval(r#"__endSuite()"#).await.unwrap();
        handle.eval(r#"__endSuite()"#).await.unwrap();

        let result = handle.eval(r#"__getSummary()"#).await.unwrap();

        assert_eq!(result["passed"], serde_json::json!(1));
        // Check that test name includes suite path
        let results = result["results"].as_array().unwrap();
        assert_eq!(
            results[0]["name"],
            serde_json::json!("Math > addition > adds numbers")
        );

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_assert_equal_pass() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle.eval(r#"assertEqual(42, 42)"#).await.unwrap();
        assert_eq!(result, serde_json::json!(true));

        let result = handle
            .eval(r#"assertEqual("hello", "hello")"#)
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!(true));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_assert_equal_fail() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle.eval(r#"assertEqual(1, 2)"#).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Assertion failed"));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_assert_not_equal() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle.eval(r#"assertNotEqual(1, 2)"#).await.unwrap();
        assert_eq!(result, serde_json::json!(true));

        let result = handle.eval(r#"assertNotEqual(1, 1)"#).await;
        assert!(result.is_err());

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_assert_true_false() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        // assertTrue with truthy values
        let result = handle.eval(r#"assertTrue(true)"#).await.unwrap();
        assert_eq!(result, serde_json::json!(true));

        let result = handle.eval(r#"assertTrue(1)"#).await.unwrap();
        assert_eq!(result, serde_json::json!(true));

        let result = handle.eval(r#"assertTrue("hello")"#).await.unwrap();
        assert_eq!(result, serde_json::json!(true));

        // assertTrue with falsy values should fail
        let result = handle.eval(r#"assertTrue(false)"#).await;
        assert!(result.is_err());

        // assertFalse with falsy values
        let result = handle.eval(r#"assertFalse(false)"#).await.unwrap();
        assert_eq!(result, serde_json::json!(true));

        let result = handle.eval(r#"assertFalse(0)"#).await.unwrap();
        assert_eq!(result, serde_json::json!(true));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_assert_ok() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle.eval(r#"assertOk(42)"#).await.unwrap();
        assert_eq!(result, serde_json::json!(true));

        let result = handle.eval(r#"assertOk("value")"#).await.unwrap();
        assert_eq!(result, serde_json::json!(true));

        let result = handle.eval(r#"assertOk(null)"#).await;
        assert!(result.is_err());

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_assert_deep_equal() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle
            .eval(r#"assertDeepEqual({a: 1, b: 2}, {a: 1, b: 2})"#)
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!(true));

        let result = handle
            .eval(r#"assertDeepEqual([1, 2, 3], [1, 2, 3])"#)
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!(true));

        let result = handle.eval(r#"assertDeepEqual({a: 1}, {a: 2})"#).await;
        assert!(result.is_err());

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_reset() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        // Record some tests using internal ops
        handle
            .eval(r#"__recordResult("test1", true, 5, null)"#)
            .await
            .unwrap();
        handle
            .eval(r#"__recordResult("test2", false, 3, "error")"#)
            .await
            .unwrap();

        // Reset
        handle.eval(r#"__resetTests()"#).await.unwrap();

        // Summary should be empty
        let result = handle.eval(r#"__getSummary()"#).await.unwrap();
        assert_eq!(result["passed"], serde_json::json!(0));
        assert_eq!(result["failed"], serde_json::json!(0));
        assert_eq!(result["total"], serde_json::json!(0));

        engine.shutdown().await;
    }

    // ============ Full JS API Tests ============

    #[tokio::test]
    async fn test_js_wrapper_functions_exist() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        // Verify describe function exists and is callable
        let result = handle.eval(r#"typeof describe"#).await.unwrap();
        assert_eq!(
            result,
            serde_json::json!("function"),
            "describe should be a function"
        );

        let result = handle.eval(r#"typeof it"#).await.unwrap();
        assert_eq!(
            result,
            serde_json::json!("function"),
            "it should be a function"
        );

        let result = handle.eval(r#"typeof test"#).await.unwrap();
        assert_eq!(
            result,
            serde_json::json!("function"),
            "test should be a function"
        );

        let result = handle.eval(r#"typeof run"#).await.unwrap();
        assert_eq!(
            result,
            serde_json::json!("function"),
            "run should be a function"
        );

        let result = handle.eval(r#"typeof assert"#).await.unwrap();
        assert_eq!(
            result,
            serde_json::json!("object"),
            "assert should be an object"
        );

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_describe_with_callback() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        // Test describe with callback function
        let result = handle
            .eval(
                r#"
                describe('Math', () => {
                    it('adds numbers', () => {
                        assert.equal(1 + 1, 2);
                    });
                });
                run()
            "#,
            )
            .await
            .unwrap();

        assert_eq!(result["passed"], serde_json::json!(1));
        assert_eq!(result["failed"], serde_json::json!(0));
        assert_eq!(result["total"], serde_json::json!(1));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_nested_describe() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle
            .eval(
                r#"
                describe('Math', () => {
                    describe('addition', () => {
                        it('adds positive numbers', () => {
                            assert.equal(1 + 2, 3);
                        });
                        it('adds negative numbers', () => {
                            assert.equal(-1 + -2, -3);
                        });
                    });
                    describe('subtraction', () => {
                        it('subtracts numbers', () => {
                            assert.equal(5 - 3, 2);
                        });
                    });
                });
                run()
            "#,
            )
            .await
            .unwrap();

        assert_eq!(result["passed"], serde_json::json!(3));
        assert_eq!(result["failed"], serde_json::json!(0));
        assert_eq!(result["total"], serde_json::json!(3));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_it_skip() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle
            .eval(
                r#"
                describe('Tests', () => {
                    it('runs this', () => {
                        assert.ok(true);
                    });
                    it.skip('skips this', () => {
                        assert.ok(false); // Would fail if run
                    });
                });
                run()
            "#,
            )
            .await
            .unwrap();

        assert_eq!(result["passed"], serde_json::json!(1));
        assert_eq!(result["skipped"], serde_json::json!(1));
        assert_eq!(result["failed"], serde_json::json!(0));
        assert_eq!(result["total"], serde_json::json!(2));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_it_only() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle
            .eval(
                r#"
                describe('Tests', () => {
                    it('will be skipped', () => {
                        assert.ok(true);
                    });
                    it.only('runs only this', () => {
                        assert.equal(2 + 2, 4);
                    });
                    it('also skipped', () => {
                        assert.ok(true);
                    });
                });
                run()
            "#,
            )
            .await
            .unwrap();

        assert_eq!(result["passed"], serde_json::json!(1));
        assert_eq!(result["skipped"], serde_json::json!(2));
        assert_eq!(result["failed"], serde_json::json!(0));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_before_each_hook() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle
            .eval(
                r#"
                let counter = 0;

                describe('Hooks', () => {
                    beforeEach(() => {
                        counter++;
                    });

                    it('first test', () => {
                        assert.equal(counter, 1);
                    });

                    it('second test', () => {
                        assert.equal(counter, 2);
                    });
                });
                run()
            "#,
            )
            .await
            .unwrap();

        assert_eq!(result["passed"], serde_json::json!(2));
        assert_eq!(result["failed"], serde_json::json!(0));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_after_each_hook() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle
            .eval(
                r#"
                let cleanupCount = 0;

                describe('Cleanup', () => {
                    afterEach(() => {
                        cleanupCount++;
                    });

                    it('test 1', () => {
                        assert.ok(true);
                    });

                    it('test 2', () => {
                        assert.ok(true);
                    });
                });

                run()
            "#,
            )
            .await
            .unwrap();

        // Verify tests passed
        assert_eq!(result["passed"], serde_json::json!(2));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_failing_test() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle
            .eval(
                r#"
                describe('Failures', () => {
                    it('this test fails', () => {
                        assert.equal(1, 2);
                    });
                    it('this test passes', () => {
                        assert.ok(true);
                    });
                });
                run()
            "#,
            )
            .await
            .unwrap();

        assert_eq!(result["passed"], serde_json::json!(1));
        assert_eq!(result["failed"], serde_json::json!(1));
        assert_eq!(result["total"], serde_json::json!(2));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_assert_throws() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        // assert.throws is async, so tests using it need to be async too
        let result = handle
            .eval(
                r#"
                describe('assert.throws', () => {
                    it('catches thrown errors', async () => {
                        await assert.throws(() => {
                            throw new Error('expected error');
                        });
                    });

                    it('validates error message', async () => {
                        await assert.throws(() => {
                            throw new Error('something went wrong');
                        }, 'wrong');
                    });
                });
                run()
            "#,
            )
            .await
            .unwrap();

        assert_eq!(result["passed"], serde_json::json!(2));
        assert_eq!(result["failed"], serde_json::json!(0));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_async_tests() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        // Test that async tests work correctly
        let result = handle
            .eval(
                r#"
                describe('Async tests', () => {
                    it('handles async/await', async () => {
                        const result = await Promise.resolve(42);
                        assert.equal(result, 42);
                    });

                    it('handles promise chains', () => {
                        return Promise.resolve('test').then(value => {
                            assert.equal(value, 'test');
                        });
                    });

                    it('handles delayed promises', async () => {
                        const delay = (ms) => new Promise(r => setTimeout(r, ms));
                        await delay(10);
                        assert.ok(true);
                    });
                });
                run()
            "#,
            )
            .await
            .unwrap();

        assert_eq!(result["passed"], serde_json::json!(3));
        assert_eq!(result["failed"], serde_json::json!(0));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_test_alias() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        // test() is an alias for it()
        let result = handle
            .eval(
                r#"
                describe('Using test()', () => {
                    test('works like it()', () => {
                        assert.ok(true);
                    });
                    test.skip('skips like it.skip()', () => {
                        assert.ok(false);
                    });
                });
                run()
            "#,
            )
            .await
            .unwrap();

        assert_eq!(result["passed"], serde_json::json!(1));
        assert_eq!(result["skipped"], serde_json::json!(1));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_deep_equal_in_suite() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        let result = handle
            .eval(
                r#"
                describe('deepEqual', () => {
                    it('compares objects', () => {
                        assert.deepEqual({a: 1, b: 2}, {a: 1, b: 2});
                    });
                    it('compares arrays', () => {
                        assert.deepEqual([1, 2, 3], [1, 2, 3]);
                    });
                    it('compares nested structures', () => {
                        assert.deepEqual(
                            {arr: [1, 2], obj: {x: 'y'}},
                            {arr: [1, 2], obj: {x: 'y'}}
                        );
                    });
                });
                run()
            "#,
            )
            .await
            .unwrap();

        assert_eq!(result["passed"], serde_json::json!(3));
        assert_eq!(result["failed"], serde_json::json!(0));

        engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_standalone_it() {
        let engine = Engine::builder()
            .pool_size(1)
            .extension(create_test_extension())
            .build()
            .unwrap();

        let handle = engine.handle();

        // Tests without describe block
        let result = handle
            .eval(
                r#"
                it('works without describe', () => {
                    assert.equal(10 + 5, 15);
                });

                test('also works standalone', () => {
                    assert.ok('hello');
                });

                run()
            "#,
            )
            .await
            .unwrap();

        assert_eq!(result["passed"], serde_json::json!(2));
        assert_eq!(result["failed"], serde_json::json!(0));

        engine.shutdown().await;
    }
}
