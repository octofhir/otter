use otter_engine::{CapabilitiesBuilder, EngineBuilder, NodeApiProfile, Otter};
use tempfile::tempdir;

fn js_string(input: &str) -> String {
    serde_json::to_string(input).unwrap()
}

fn eval_ok(otter: &mut Otter, code: &str) {
    let value = otter
        .eval_sync(code)
        .unwrap_or_else(|e| panic!("Eval failed: {e}"));
    let out = value.as_string().map(|s| s.to_string()).unwrap_or_default();
    assert_eq!(out, "ok");
}

fn eval_err(otter: &mut Otter, code: &str, needle: &str) {
    let err = otter
        .eval_sync(code)
        .expect_err("Expected eval to fail")
        .to_string();
    assert!(
        err.contains(needle),
        "Expected error to contain '{needle}', got '{err}'"
    );
}

#[test]
fn test_fs_read_denied_without_read_capability() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("data.txt");
    std::fs::write(&file, "hello").unwrap();

    let mut otter = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::Full)
        .build();
    let code = format!(
        "import fs from 'node:fs'; fs.readFileSync({}, 'utf8');",
        js_string(&file.to_string_lossy())
    );
    eval_err(&mut otter, &code, "PermissionDenied");
}

#[test]
fn test_fs_read_allowed_with_read_capability() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("data.txt");
    std::fs::write(&file, "hello").unwrap();

    let caps = CapabilitiesBuilder::new()
        .allow_read(vec![dir.path().to_path_buf()])
        .build();
    let mut otter = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::Full)
        .capabilities(caps)
        .build();
    let code = format!(
        "import fs from 'node:fs'; if (fs.readFileSync({}, 'utf8') !== 'hello') throw new Error('bad'); 'ok';",
        js_string(&file.to_string_lossy())
    );
    eval_ok(&mut otter, &code);
}

#[test]
fn test_fs_write_denied_without_write_capability() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("write.txt");

    let caps = CapabilitiesBuilder::new()
        .allow_read(vec![dir.path().to_path_buf()])
        .build();
    let mut otter = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::Full)
        .capabilities(caps)
        .build();
    let code = format!(
        "import fs from 'node:fs'; fs.writeFileSync({}, 'x');",
        js_string(&file.to_string_lossy())
    );
    eval_err(&mut otter, &code, "PermissionDenied");
}

#[test]
fn test_fs_write_allowed_with_write_capability() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("write.txt");

    let caps = CapabilitiesBuilder::new()
        .allow_write(vec![dir.path().to_path_buf()])
        .build();
    let mut otter = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::Full)
        .capabilities(caps)
        .build();
    let code = format!(
        "import fs from 'node:fs'; fs.writeFileSync({}, 'x'); 'ok';",
        js_string(&file.to_string_lossy())
    );
    eval_ok(&mut otter, &code);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "x");
}

#[test]
fn test_fs_readdir_denied_without_read_capability() {
    let dir = tempdir().unwrap();

    let mut otter = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::Full)
        .build();
    let code = format!(
        "import fs from 'node:fs'; fs.readdirSync({});",
        js_string(&dir.path().to_string_lossy())
    );
    eval_err(&mut otter, &code, "PermissionDenied");
}

#[test]
fn test_fs_mkdir_denied_without_write_capability() {
    let dir = tempdir().unwrap();
    let subdir = dir.path().join("new-dir");

    let caps = CapabilitiesBuilder::new()
        .allow_read(vec![dir.path().to_path_buf()])
        .build();
    let mut otter = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::Full)
        .capabilities(caps)
        .build();
    let code = format!(
        "import fs from 'node:fs'; fs.mkdirSync({});",
        js_string(&subdir.to_string_lossy())
    );
    eval_err(&mut otter, &code, "PermissionDenied");
}

#[test]
fn test_process_chdir_denied_without_run_capability() {
    let mut otter = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::Full)
        .build();
    eval_err(
        &mut otter,
        "import process from 'node:process'; process.chdir('.');",
        "PermissionDenied",
    );
}

#[test]
fn test_process_exit_fail_closed_even_with_run_capability() {
    let caps = CapabilitiesBuilder::new().allow_subprocess().build();
    let mut otter = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::Full)
        .capabilities(caps)
        .build();
    eval_err(
        &mut otter,
        "import process from 'node:process'; process.exit(7);",
        "ProcessExit",
    );
}

#[test]
fn test_process_hrtime_denied_without_hrtime_capability() {
    let mut otter = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::Full)
        .build();
    eval_err(
        &mut otter,
        "import process from 'node:process'; process.hrtime();",
        "PermissionDenied",
    );
}

#[test]
fn test_process_hrtime_allowed_with_capability() {
    let caps = CapabilitiesBuilder::new().allow_hrtime().build();
    let mut otter = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::Full)
        .capabilities(caps)
        .build();
    eval_ok(
        &mut otter,
        "import process from 'node:process'; const t = process.hrtime(); if (!Array.isArray(t) || t.length !== 2) throw new Error('bad'); 'ok';",
    );
}

#[test]
fn test_process_env_reads_from_env_store_when_allowed() {
    let caps = CapabilitiesBuilder::new()
        .allow_env(vec!["NODE_ENV".into()])
        .build();
    let mut otter = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::Full)
        .capabilities(caps)
        .env(|b| b.explicit("NODE_ENV", "production"))
        .build();
    eval_ok(
        &mut otter,
        "import process from 'node:process'; if (process.env.NODE_ENV !== 'production') throw new Error('env read'); if (!('NODE_ENV' in process.env)) throw new Error('env has'); if (!Object.keys(process.env).includes('NODE_ENV')) throw new Error('env keys'); 'ok';",
    );
}

#[test]
fn test_process_env_hides_denied_vars_from_get_has_and_keys() {
    let caps = CapabilitiesBuilder::new()
        .allow_env(vec!["NODE_ENV".into()])
        .build();
    let mut otter = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::Full)
        .capabilities(caps)
        .env(|b| {
            b.explicit("NODE_ENV", "production")
                .explicit("SECRET_TOKEN", "leak")
        })
        .build();
    eval_ok(
        &mut otter,
        "import process from 'node:process'; if (process.env.SECRET_TOKEN !== undefined) throw new Error('env get leak'); if ('SECRET_TOKEN' in process.env) throw new Error('env has leak'); if (Object.keys(process.env).includes('SECRET_TOKEN')) throw new Error('env key leak'); 'ok';",
    );
}
