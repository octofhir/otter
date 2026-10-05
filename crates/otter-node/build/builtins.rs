//! Current-codec producer for every builtin CommonJS module and the Node
//! realm installer.
//!
//! # Contents
//! - `BUILTINS`, this producer's reading of `src/builtin_table.rs`.
//! - Body, wrapper and installer text assembly from the table's files.
//! - Default-compiler compilation, full verification and artifact emission into
//!   `$OUT_DIR/builtins/<name>.{js,bytecode}` and
//!   `$OUT_DIR/node-realm-installer.{js,bytecode}`.
//!
//! # Invariants
//! - A builtin compiles exactly as the runtime CommonJS loader would compile
//!   it: the `otter_bytecode::commonjs` wrapper around the body, eval-goal
//!   compiled under `<eval>` and then named by the builtin's URL (module and
//!   every function). The emitted `.js` is that complete wrapper text.
//! - The realm installer is one classic script under `<realm-installer>`.
//! - Every artifact passes the bounded codec and the full verifier before it is
//!   written; any failure stops the build. Artifacts carry no addresses, cache
//!   keys or format versions.
//!
//! # See also
//! - `src/builtin_table.rs` for the rows.
//! - `otter_vm::EmbeddedCommonJs` / `otter_runtime::ExtensionJs` for the
//!   runtime owners of the artifacts.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Builtin {
    name: &'static str,
    url: &'static str,
    vendored: bool,
    paths: &'static [&'static str],
}

macro_rules! builtin_table {
    ($($role:ident $name:ident $url:literal $body:ident $($path:literal)+;)*) => {
        const BUILTINS: &[Builtin] = &[$(Builtin {
            name: stringify!($name),
            url: $url,
            vendored: builtin_table!(@vendored $body),
            paths: &[$($path),+],
        }),*];
    };
    (@vendored vendored) => { true };
    (@vendored plain) => { false };
}

include!("../src/builtin_table.rs");

/// The line a vendored `lib/` file is compiled behind: the contract Node's
/// native wrapper provides, from the compat realm. It shares line 1 with the
/// wrapper prologue, so the file's own lines keep their numbers.
const REALM_PREFIX: &str =
    "'use strict';const { primordials, internalBinding } = require('internal/bootstrap/realm');\n";

/// The Node realm installer's constituent files, concatenated as written.
const INSTALLER_PATHS: &[&str] = &[
    "node_process_globals.js",
    "node_console_global.js",
    "process_stdio_global.js",
    "node_timers_global.js",
    "node_promise_rejection_global.js",
];
const INSTALLER_SPECIFIER: &str = "<realm-installer>";

const MAX_BYTECODE_BYTES: usize = 16 * 1024 * 1024;

type BuildError = Box<dyn std::error::Error + Send + Sync>;

pub(crate) fn generate(out: &Path, src: &Path) -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build/builtins.rs");
    println!("cargo:rerun-if-changed=src/builtin_table.rs");
    for path in BUILTINS
        .iter()
        .flat_map(|builtin| builtin.paths.iter())
        .chain(INSTALLER_PATHS)
    {
        println!("cargo:rerun-if-changed=src/{path}");
    }
    let dir = out.join("builtins");
    std::fs::create_dir_all(&dir)?;

    let next = AtomicUsize::new(0);
    let failures = Mutex::new(Vec::new());
    let workers = std::thread::available_parallelism().map_or(4, usize::from);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(builtin) = BUILTINS.get(index) else {
                        break;
                    };
                    if let Err(error) = emit_builtin(src, &dir, builtin) {
                        failures
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("{} ({}): {error}", builtin.name, builtin.url));
                    }
                }
            });
        }
    });
    let failures = failures
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !failures.is_empty() {
        return Err(failures.join("\n").into());
    }
    emit_installer(src, out).map_err(|error| -> Box<dyn std::error::Error> { error })
}

fn read(src: &Path, path: &str) -> Result<String, BuildError> {
    std::fs::read_to_string(src.join(path)).map_err(|error| format!("src/{path}: {error}").into())
}

fn body(src: &Path, builtin: &Builtin) -> Result<String, BuildError> {
    if builtin.vendored {
        let [path] = builtin.paths else {
            return Err("a vendored row names exactly one file".into());
        };
        return Ok(format!("{REALM_PREFIX}{}", read(src, path)?));
    }
    let parts = builtin
        .paths
        .iter()
        .map(|path| read(src, path))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(parts.join("\n"))
}

fn emit_builtin(src: &Path, dir: &Path, builtin: &Builtin) -> Result<(), BuildError> {
    let source = otter_bytecode::commonjs::wrapper_source(&body(src, builtin)?);
    let mut module = otter_compiler::compile_eval_source(
        &source,
        otter_syntax::SourceKind::JavaScript,
        "<eval>",
        false,
        false,
        None,
        false,
        false,
        false,
        false,
        false,
    )?;
    module.module = builtin.url.to_owned();
    for function in &mut module.functions {
        function.module_url = builtin.url.to_owned();
    }
    let encoded = otter_bytecode::binary::encode_module_bounded(&module, MAX_BYTECODE_BYTES)?;
    otter_bytecode::binary::decode_module(&encoded)?;
    std::fs::write(dir.join(format!("{}.js", builtin.name)), source)?;
    std::fs::write(dir.join(format!("{}.bytecode", builtin.name)), encoded)?;
    Ok(())
}

fn emit_installer(src: &Path, out: &Path) -> Result<(), BuildError> {
    let text = INSTALLER_PATHS
        .iter()
        .map(|path| read(src, path))
        .collect::<Result<String, _>>()?;
    let compiled = otter_compiler::compile_script_source_to_module(
        &text,
        otter_syntax::SourceKind::JavaScript,
        INSTALLER_SPECIFIER,
    )?;
    let encoded =
        otter_bytecode::binary::encode_module_bounded(&compiled.bytecode, MAX_BYTECODE_BYTES)?;
    otter_bytecode::binary::decode_module(&encoded)?;
    std::fs::write(out.join("node-realm-installer.js"), text)?;
    std::fs::write(out.join("node-realm-installer.bytecode"), encoded)?;
    Ok(())
}
