use std::time::{SystemTime, UNIX_EPOCH};

use otter_modules::modules_extension;
use otter_runtime::{ModuleLoaderConfig, ObjectHandle, OtterRuntime, RegisterValue};

fn temp_test_dir(name: &str) -> std::path::PathBuf {
    let mut dir = std::env::temp_dir();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should move forward")
        .as_nanos();
    dir.push(format!("otter-modules-{name}-{unique}"));
    std::fs::create_dir_all(&dir).expect("temp dir should exist");
    dir
}

fn namespace_property(
    runtime: &mut OtterRuntime,
    value: RegisterValue,
    name: &str,
) -> RegisterValue {
    let object = value
        .as_object_handle()
        .map(ObjectHandle)
        .expect("value should be an object");
    let property = runtime.state_mut().intern_property_name(name);
    runtime
        .state_mut()
        .own_property_value(object, property)
        .expect("object property should be readable")
}

#[test]
fn esm_can_import_otter_sql_module() {
    let dir = temp_test_dir("esm-sql");
    std::fs::write(
        dir.join("main.mjs"),
        "import openSql, { sql } from 'otter:sql'; const db = openSql(':memory:'); db.execute('CREATE TABLE users (name TEXT, meta TEXT)', []); db.execute('INSERT INTO users (name, meta) VALUES (?, ?)', ['Ada', { active: true }]); const rows = sql(':memory:'); rows.execute('CREATE TABLE tmp (value INTEGER)', []); rows.execute('INSERT INTO tmp (value) VALUES (?)', [2]); const first = db.query('SELECT name, meta FROM users', [])[0]; const second = rows.query('SELECT value FROM tmp', [])[0]; export default (first.meta.active ? first.name.length : 0) + second.value;",
    )
    .expect("main should write");

    let mut runtime = OtterRuntime::builder()
        .module_loader(ModuleLoaderConfig {
            base_dir: dir,
            ..Default::default()
        })
        .extension(modules_extension())
        .build();

    let result = runtime
        .run_entry_specifier("./main.mjs", None)
        .expect("esm otter:sql import should execute");

    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "default"),
        RegisterValue::from_i32(5)
    );
}

#[test]
fn commonjs_can_require_otter_sql_module() {
    let dir = temp_test_dir("cjs-sql");
    std::fs::write(
        dir.join("main.cjs"),
        "const openSql = require('otter:sql').default; const db = openSql(':memory:'); db.execute('CREATE TABLE items (value INTEGER)', []); db.execute('INSERT INTO items (value) VALUES (?)', [42]); module.exports = db.query('SELECT value FROM items', [])[0].value;",
    )
    .expect("main should write");

    let mut runtime = OtterRuntime::builder()
        .module_loader(ModuleLoaderConfig {
            base_dir: dir,
            ..Default::default()
        })
        .extension(modules_extension())
        .build();

    let result = runtime
        .run_entry_specifier("./main.cjs", None)
        .expect("cjs otter:sql require should execute");

    assert_eq!(result.return_value(), RegisterValue::from_i32(42));
}

#[test]
fn sql_database_exposes_management_properties_and_close_state() {
    let dir = temp_test_dir("sql-management");
    std::fs::write(
        dir.join("main.mjs"),
        "import openSql from 'otter:sql'; const db = openSql(':memory:'); db.execute('CREATE TABLE items (value INTEGER)', []); db.execute('INSERT INTO items (value) VALUES (?)', [7]); const before = db.query('SELECT value FROM items', [])[0].value + (db.adapter === 'sqlite' ? 10 : 0) + (db.isMemory ? 100 : 0) + (db.closed ? 1000 : 0) + db.path.length; db.close(); export default before + (db.closed ? 10000 : 0);",
    )
    .expect("main should write");

    let mut runtime = OtterRuntime::builder()
        .module_loader(ModuleLoaderConfig {
            base_dir: dir,
            ..Default::default()
        })
        .extension(modules_extension())
        .build();

    let result = runtime
        .run_entry_specifier("./main.mjs", None)
        .expect("sql management script should execute");

    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "default"),
        RegisterValue::from_i32(10125)
    );
}

#[test]
fn sql_database_throws_after_close() {
    let dir = temp_test_dir("sql-close-error");
    std::fs::write(
        dir.join("main.mjs"),
        "import { openSql } from 'otter:sql'; const db = openSql(':memory:'); db.close(); db.query('SELECT 1', []); export default 1;",
    )
    .expect("main should write");

    let mut runtime = OtterRuntime::builder()
        .module_loader(ModuleLoaderConfig {
            base_dir: dir,
            ..Default::default()
        })
        .extension(modules_extension())
        .build();

    let error = runtime
        .run_entry_specifier("./main.mjs", None)
        .expect_err("closed sql database should throw");
    assert!(
        error
            .to_string()
            .contains("TypeError: SQL database is closed")
    );
}

#[test]
fn sql_database_supports_query_helpers_and_execute_metadata() {
    let dir = temp_test_dir("sql-helpers");
    std::fs::write(
        dir.join("main.mjs"),
        "import openSql from 'otter:sql'; const db = openSql(':memory:'); db.execute('CREATE TABLE items (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, score INTEGER)', []); const meta = db.executeMeta('INSERT INTO items (name, score) VALUES (?, ?)', ['otter', 40]); db.execute('INSERT INTO items (name, score) VALUES (?, ?)', ['vm', 2]); const one = db.queryOne('SELECT name, score FROM items WHERE name = ?', ['otter']); const value = db.queryValue('SELECT score FROM items WHERE name = ?', ['vm']); export default one.score + value + meta.rowsAffected + db.lastInsertRowId;",
    )
    .expect("main should write");

    let mut runtime = OtterRuntime::builder()
        .module_loader(ModuleLoaderConfig {
            base_dir: dir,
            ..Default::default()
        })
        .extension(modules_extension())
        .build();

    let result = runtime
        .run_entry_specifier("./main.mjs", None)
        .expect("sql helper script should execute");

    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "default"),
        RegisterValue::from_i32(45)
    );
}

#[test]
fn sql_database_supports_transactions() {
    let dir = temp_test_dir("sql-transactions");
    std::fs::write(
        dir.join("main.mjs"),
        "import { openSql } from 'otter:sql'; const db = openSql(':memory:'); db.execute('CREATE TABLE items (value INTEGER)', []); db.begin(); db.execute('INSERT INTO items (value) VALUES (?)', [1]); const during = db.inTransaction ? 10 : 0; db.rollback(); db.begin(); db.execute('INSERT INTO items (value) VALUES (?)', [32]); db.commit(); const count = db.queryValue('SELECT COUNT(*) FROM items', []); const sum = db.queryValue('SELECT SUM(value) FROM items', []); export default during + count + sum;",
    )
    .expect("main should write");

    let mut runtime = OtterRuntime::builder()
        .module_loader(ModuleLoaderConfig {
            base_dir: dir,
            ..Default::default()
        })
        .extension(modules_extension())
        .build();

    let result = runtime
        .run_entry_specifier("./main.mjs", None)
        .expect("sql transaction script should execute");

    assert_eq!(
        namespace_property(&mut runtime, result.return_value(), "default"),
        RegisterValue::from_i32(43)
    );
}
