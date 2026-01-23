//! Otter CLI - A fast TypeScript/JavaScript runtime.
//!
//! VM-based JavaScript execution with pluggable builtins.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::filter::EnvFilter;

use otter_vm_builtins::create_builtins_extension;
use otter_vm_compiler::Compiler;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::runtime::VmRuntime;
use otter_vm_core::value::Value;
use otter_vm_runtime::{ExtensionRegistry, OpHandler};

mod config;

#[derive(Parser)]
#[command(
    name = "otter",
    version,
    about = "A fast TypeScript/JavaScript runtime",
    long_about = "Otter is a secure, fast TypeScript/JavaScript runtime powered by a custom VM."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Script file to run (shorthand for `otter run <file>`)
    #[arg(value_name = "FILE")]
    file: Option<PathBuf>,

    /// Evaluate argument as a script
    #[arg(short = 'e', long = "eval")]
    eval: Option<String>,

    /// Verbose output
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Config file path
    #[arg(long, global = true)]
    config: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a JavaScript/TypeScript file
    Run {
        /// The script file to run
        file: PathBuf,
    },
    /// Show runtime information
    Info,
    /// Initialize a new project
    Init,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("warn".parse()?))
        .init();

    let cli = Cli::parse();

    // Handle --eval flag
    if let Some(code) = cli.eval {
        return run_code(&code, "<eval>");
    }

    // Handle direct file argument (otter script.js)
    if let Some(file) = cli.file {
        return run_file(&file);
    }

    match cli.command {
        Some(Commands::Run { file }) => run_file(&file),
        Some(Commands::Info) => {
            println!("Otter Runtime");
            println!("Version: {}", env!("CARGO_PKG_VERSION"));
            println!("VM: otter-vm-core");
            println!("Platform: {}", std::env::consts::OS);
            println!("Arch: {}", std::env::consts::ARCH);
            Ok(())
        }
        Some(Commands::Init) => {
            init_project()?;
            Ok(())
        }
        None => {
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

/// Run a JavaScript file
fn run_file(path: &PathBuf) -> Result<()> {
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read file: {}", path.display()))?;

    let source_url = path.to_string_lossy();
    run_code(&source, &source_url)
}

/// Run JavaScript code
fn run_code(source: &str, source_url: &str) -> Result<()> {
    // Create extension registry with builtins
    let mut registry = ExtensionRegistry::new();
    let builtins = create_builtins_extension();
    registry
        .register(builtins)
        .map_err(|e| anyhow::anyhow!("Failed to register builtins: {}", e))?;

    // Create the VM runtime
    let runtime = VmRuntime::new();

    // Create a context with builtins set up
    let mut ctx = runtime.create_context();

    // Wire up extension ops as global native functions
    setup_extension_globals(&mut ctx, &registry);

    // Run extension JS setup code (this creates Object, console, etc.)
    run_extension_js(&registry, &runtime, &mut ctx)?;

    // Compile user code
    let compiler = Compiler::new();
    let module = compiler
        .compile(source, source_url)
        .map_err(|e| anyhow::anyhow!("Compilation error: {:?}", e))?;

    // Execute the module
    let result = runtime
        .execute_module_with_context(&module, &mut ctx)
        .map_err(|e| anyhow::anyhow!("Runtime error: {:?}", e))?;

    // Print result if it's not undefined
    if !result.is_undefined() {
        println!("{}", format_value(&result));
    }

    Ok(())
}

/// Set up extension ops as global functions
fn setup_extension_globals(
    ctx: &mut otter_vm_core::context::VmContext,
    registry: &ExtensionRegistry,
) {
    for op_name in registry.op_names() {
        if let Some(handler) = registry.get_op(op_name) {
            let handler = handler.clone();
            let native_fn = move |args: &[Value]| -> Result<Value, String> {
                match &handler {
                    OpHandler::Native(f) => f(args),
                    OpHandler::Sync(f) => {
                        // Convert VmValue to JsonValue
                        let json_args: Vec<serde_json::Value> =
                            args.iter().map(|v| vm_value_to_json(v)).collect();

                        // Call the handler
                        let result = f(&json_args)?;

                        // Convert JsonValue back to VmValue
                        Ok(json_to_vm_value(&result))
                    }
                    OpHandler::Async(_) => {
                        // Async ops not yet supported in sync context
                        Err("Async operations not supported in sync context".to_string())
                    }
                }
            };
            ctx.set_global(op_name, Value::native_function(native_fn));
        }
    }
}

/// Run extension JS setup code
fn run_extension_js(
    registry: &ExtensionRegistry,
    runtime: &VmRuntime,
    ctx: &mut otter_vm_core::context::VmContext,
) -> Result<()> {
    for js_code in registry.all_js() {
        let compiler = Compiler::new();
        match compiler.compile(js_code, "<builtin>") {
            Ok(module) => {
                runtime
                    .execute_module_with_context(&module, ctx)
                    .map_err(|e| anyhow::anyhow!("Failed to execute builtin JS: {:?}", e))?;
            }
            Err(e) => {
                // Log warning but continue - builtins may use unsupported features
                tracing::warn!(
                    "Could not compile builtin JS (some features may be unavailable): {:?}",
                    e
                );
            }
        }
    }
    Ok(())
}

/// Convert VM Value to JSON Value
fn vm_value_to_json(value: &Value) -> serde_json::Value {
    use serde_json::json;

    if value.is_undefined() {
        return json!(null);
    }

    if value.is_null() {
        return json!(null);
    }

    if let Some(b) = value.as_boolean() {
        return json!(b);
    }

    if let Some(n) = value.as_number() {
        if n.is_nan() || n.is_infinite() {
            return json!(null);
        }
        return json!(n);
    }

    if let Some(s) = value.as_string() {
        return json!(s.as_str());
    }

    if let Some(obj) = value.as_object() {
        // Check if array
        if obj.is_array() {
            let len = obj
                .get(&PropertyKey::string("length"))
                .and_then(|v| v.as_int32())
                .unwrap_or(0) as usize;

            let mut arr = Vec::with_capacity(len);
            for i in 0..len {
                if let Some(elem) = obj.get(&PropertyKey::Index(i as u32)) {
                    arr.push(vm_value_to_json(&elem));
                } else {
                    arr.push(json!(null));
                }
            }
            return serde_json::Value::Array(arr);
        }

        // Regular object
        let mut map = serde_json::Map::new();
        for key in obj.own_keys() {
            if let Some(val) = obj.get(&key) {
                let key_str = match key {
                    PropertyKey::String(s) => s.as_str().to_string(),
                    PropertyKey::Index(i) => i.to_string(),
                    PropertyKey::Symbol(_) => continue,
                };
                map.insert(key_str, vm_value_to_json(&val));
            }
        }
        return serde_json::Value::Object(map);
    }

    json!(null)
}

/// Convert JSON Value to VM Value
fn json_to_vm_value(json: &serde_json::Value) -> Value {
    use otter_vm_core::string::JsString;

    match json {
        serde_json::Value::Null => Value::null(),
        serde_json::Value::Bool(b) => Value::boolean(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                if i >= i32::MIN as i64 && i <= i32::MAX as i64 {
                    Value::int32(i as i32)
                } else {
                    Value::number(i as f64)
                }
            } else {
                Value::number(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_json::Value::String(s) => Value::string(JsString::intern(s)),
        serde_json::Value::Array(arr) => {
            let obj = Arc::new(JsObject::array(arr.len()));
            for (i, elem) in arr.iter().enumerate() {
                obj.set(PropertyKey::Index(i as u32), json_to_vm_value(elem));
            }
            Value::object(obj)
        }
        serde_json::Value::Object(map) => {
            let obj = Arc::new(JsObject::new(None));
            for (key, val) in map {
                obj.set(PropertyKey::string(key), json_to_vm_value(val));
            }
            Value::object(obj)
        }
    }
}

/// Format a Value for display
fn format_value(value: &Value) -> String {
    if value.is_undefined() {
        return "undefined".to_string();
    }

    if value.is_null() {
        return "null".to_string();
    }

    if let Some(b) = value.as_boolean() {
        return b.to_string();
    }

    if let Some(n) = value.as_number() {
        if n.is_nan() {
            return "NaN".to_string();
        }
        if n.is_infinite() {
            return if n.is_sign_positive() {
                "Infinity"
            } else {
                "-Infinity"
            }
            .to_string();
        }
        if n.fract() == 0.0 && n.abs() < 1e15 {
            return format!("{}", n as i64);
        }
        return format!("{}", n);
    }

    if let Some(s) = value.as_string() {
        return format!("'{}'", s.as_str());
    }

    if let Some(obj) = value.as_object() {
        if obj.is_array() {
            let len = obj
                .get(&PropertyKey::string("length"))
                .and_then(|v| v.as_int32())
                .unwrap_or(0);
            return format!("[Array({})]", len);
        }
        return "[object Object]".to_string();
    }

    if value.is_function() {
        return "[Function]".to_string();
    }

    "[unknown]".to_string()
}

/// Initialize a new project
fn init_project() -> Result<()> {
    use std::fs;

    // Create package.json
    let package_json = r#"{
  "name": "my-otter-project",
  "version": "1.0.0",
  "main": "index.js",
  "scripts": {
    "start": "otter run index.js"
  }
}
"#;

    // Create index.js
    let index_js = r#"// Welcome to Otter!
console.log("Hello from Otter!");

const sum = (a, b) => a + b;
console.log("2 + 3 =", sum(2, 3));
"#;

    if !std::path::Path::new("package.json").exists() {
        fs::write("package.json", package_json)?;
        println!("Created package.json");
    } else {
        println!("package.json already exists, skipping");
    }

    if !std::path::Path::new("index.js").exists() {
        fs::write("index.js", index_js)?;
        println!("Created index.js");
    } else {
        println!("index.js already exists, skipping");
    }

    println!("\nProject initialized! Run 'otter run index.js' to start.");
    Ok(())
}
