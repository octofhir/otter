//! Native `node:path` extension — zero JS shims.
//!
//! All path operations implemented in pure Rust via `#[dive]` + `dive_module!`.
//! Replaces `js/node_path.js` (99 lines) with native code.

use otter_macros::dive;
use otter_vm_core::context::NativeContext;
use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::extension_v2::{OtterExtension, Profile};
use otter_vm_runtime::registration::RegistrationContext;
#[allow(unused_imports)]
use std::path::{MAIN_SEPARATOR, Path, PathBuf};

/// Auto-generated extension struct for the `node_path` module.
pub struct NodePathExtension;

/// Helper: register all path functions + properties on a namespace builder.
fn register_path_fns_and_props(
    mut ns: otter_vm_runtime::registration::ModuleNamespaceBuilder,
    sep: &str,
    delimiter: &str,
) -> otter_vm_runtime::registration::ModuleNamespaceBuilder {
    // Register all #[dive] functions
    let fns: &[fn() -> (
        &'static str,
        std::sync::Arc<
            dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
        >,
        u32,
    )] = &[
        path_join_decl,
        path_resolve_decl,
        path_dirname_decl,
        path_basename_decl,
        path_extname_decl,
        path_normalize_decl,
        path_is_absolute_decl,
        path_parse_decl,
        path_format_decl,
        path_relative_decl,
        path_to_namespaced_path_decl,
    ];
    for decl in fns {
        let (name, native_fn, length) = decl();
        ns = ns.function(name, native_fn, length);
    }
    // Properties
    ns = ns.property("sep", Value::string(JsString::intern(sep)));
    ns = ns.property("delimiter", Value::string(JsString::intern(delimiter)));
    ns
}

impl OtterExtension for NodePathExtension {
    fn name(&self) -> &str {
        "node_path"
    }

    fn profiles(&self) -> &[Profile] {
        static PROFILES: [Profile; 2] = [Profile::SafeCore, Profile::Full];
        &PROFILES
    }

    fn deps(&self) -> &[&str] {
        &[]
    }

    fn module_specifiers(&self) -> &[&str] {
        static SPECIFIERS: [&str; 2] = ["node:path", "path"];
        &SPECIFIERS
    }

    fn install(&self, _ctx: &mut RegistrationContext) -> Result<(), otter_vm_core::error::VmError> {
        Ok(())
    }

    fn load_module(
        &self,
        _specifier: &str,
        ctx: &mut RegistrationContext,
    ) -> Option<GcRef<JsObject>> {
        // Build posix sub-object (same functions, "/" sep, ":" delimiter)
        let posix_ns = register_path_fns_and_props(ctx.module_namespace(), "/", ":");
        let posix_obj = posix_ns.build();

        // Build win32 sub-object (same functions, "\\" sep, ";" delimiter)
        let win32_ns = register_path_fns_and_props(ctx.module_namespace(), "\\", ";");
        let win32_obj = win32_ns.build();

        // Build main namespace
        let mut ns = register_path_fns_and_props(ctx.module_namespace(), "/", ":");
        ns = ns.property("posix", Value::object(posix_obj));
        ns = ns.property("win32", Value::object(win32_obj));
        Some(ns.build())
    }
}

/// Create a boxed extension instance for registration.
pub fn node_path_extension() -> Box<dyn OtterExtension> {
    Box::new(NodePathExtension)
}

// ---------------------------------------------------------------------------
// #[dive] functions — one per path method
// ---------------------------------------------------------------------------

#[dive(name = "join", length = 0)]
fn path_join(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let mut path = PathBuf::new();
    let mut has_any = false;

    for arg in args {
        if arg.is_undefined() {
            continue;
        }
        let s = value_to_str(arg)?;
        if s.is_empty() {
            continue;
        }
        has_any = true;
        if s.starts_with('/') || s.starts_with('\\') {
            path = PathBuf::from(&*s);
        } else {
            path.push(&*s);
        }
    }

    if !has_any {
        return Ok(Value::string(JsString::intern(".")));
    }

    Ok(Value::string(JsString::new_gc(&normalize_path_string(
        &path.to_string_lossy(),
    ))))
}

#[dive(name = "resolve", length = 0)]
fn path_resolve(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let mut path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));

    for arg in args {
        let s = value_to_str(arg)?;
        if Path::new(&*s).is_absolute() {
            path = PathBuf::from(&*s);
        } else {
            path.push(&*s);
        }
    }

    let result = dunce::canonicalize(&path).unwrap_or_else(|_| path);
    Ok(Value::string(JsString::new_gc(&result.to_string_lossy())))
}

#[dive(name = "dirname", length = 1)]
fn path_dirname(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let p = arg_str(args, 0, "dirname")?;
    let parent = Path::new(&p)
        .parent()
        .map(|d| d.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string());
    let result = if parent.is_empty() { "." } else { &parent };
    Ok(Value::string(JsString::new_gc(result)))
}

#[dive(name = "basename", length = 1)]
fn path_basename(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let p = arg_str(args, 0, "basename")?;
    let ext = args
        .get(1)
        .filter(|v| !v.is_undefined())
        .map(|v| value_to_str(v))
        .transpose()?;

    let name = Path::new(&p)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let result = if let Some(ext) = ext {
        if name.ends_with(&*ext) {
            name[..name.len() - ext.len()].to_string()
        } else {
            name
        }
    } else {
        name
    };

    Ok(Value::string(JsString::new_gc(&result)))
}

#[dive(name = "extname", length = 1)]
fn path_extname(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let p = arg_str(args, 0, "extname")?;
    let ext = Path::new(&p)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    Ok(Value::string(JsString::new_gc(&ext)))
}

#[dive(name = "normalize", length = 1)]
fn path_normalize(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let p = arg_str(args, 0, "normalize")?;
    Ok(Value::string(JsString::new_gc(&normalize_path_string(&p))))
}

#[dive(name = "isAbsolute", length = 1)]
fn path_is_absolute(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let p = arg_str(args, 0, "isAbsolute")?;
    Ok(Value::boolean(Path::new(&p).is_absolute()))
}

#[dive(name = "parse", length = 1)]
fn path_parse(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let p_str = arg_str(args, 0, "parse")?;
    let p = Path::new(&p_str);

    let dir = p.parent().map(|d| d.to_string_lossy()).unwrap_or_default();
    let base = p
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let name = p
        .file_stem()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();

    let root = if p_str.starts_with('/') { "/" } else { "" };

    let obj = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
    let _ = obj.set(
        PropertyKey::string("root"),
        Value::string(JsString::new_gc(root)),
    );
    let _ = obj.set(
        PropertyKey::string("dir"),
        Value::string(JsString::new_gc(&dir)),
    );
    let _ = obj.set(
        PropertyKey::string("base"),
        Value::string(JsString::new_gc(&base)),
    );
    let _ = obj.set(
        PropertyKey::string("ext"),
        Value::string(JsString::new_gc(&ext)),
    );
    let _ = obj.set(
        PropertyKey::string("name"),
        Value::string(JsString::new_gc(&name)),
    );
    Ok(Value::object(obj))
}

#[dive(name = "format", length = 1)]
fn path_format(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let obj = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or_else(|| VmError::type_error("Path must be an object"))?;

    let dir = obj
        .get(&PropertyKey::string("dir"))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
    let root = obj
        .get(&PropertyKey::string("root"))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
        .unwrap_or_default();
    let base = obj
        .get(&PropertyKey::string("base"))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()));
    let name = obj
        .get(&PropertyKey::string("name"))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
        .unwrap_or_default();
    let ext = obj
        .get(&PropertyKey::string("ext"))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
        .unwrap_or_default();

    let base_part = base.unwrap_or_else(|| format!("{name}{ext}"));

    let result = if let Some(dir) = dir {
        if dir.is_empty() {
            base_part
        } else if dir.ends_with(MAIN_SEPARATOR) {
            format!("{dir}{base_part}")
        } else {
            format!("{dir}{MAIN_SEPARATOR}{base_part}")
        }
    } else {
        format!("{root}{base_part}")
    };

    Ok(Value::string(JsString::new_gc(&result)))
}

#[dive(name = "relative", length = 2)]
fn path_relative(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let from = arg_str(args, 0, "relative")?;
    let to = arg_str(args, 1, "relative")?;

    let normalize_abs = |p: &str| -> PathBuf {
        let input = Path::new(p);
        let base = if input.is_absolute() {
            input.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(input)
        };
        dunce::canonicalize(&base).unwrap_or(base)
    };

    let from_path = normalize_abs(&from);
    let to_path = normalize_abs(&to);

    let from_components: Vec<_> = from_path.components().collect();
    let to_components: Vec<_> = to_path.components().collect();

    let mut common = 0usize;
    while common < from_components.len()
        && common < to_components.len()
        && from_components[common] == to_components[common]
    {
        common += 1;
    }

    if common == 0 {
        return Ok(Value::string(JsString::new_gc(&to)));
    }

    let mut rel = PathBuf::new();
    for _ in common..from_components.len() {
        rel.push("..");
    }
    for component in to_components.iter().skip(common) {
        rel.push(component.as_os_str());
    }

    if rel.as_os_str().is_empty() {
        Ok(Value::string(JsString::intern("")))
    } else {
        Ok(Value::string(JsString::new_gc(&rel.to_string_lossy())))
    }
}

#[dive(name = "toNamespacedPath", length = 1)]
fn path_to_namespaced_path(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    // On non-Windows, toNamespacedPath returns the path unchanged
    Ok(args.first().cloned().unwrap_or(Value::undefined()))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract a string from args[i], or return a VmError.
fn arg_str(args: &[Value], i: usize, _fn_name: &str) -> Result<String, VmError> {
    let undef = Value::undefined();
    let v = args.get(i).unwrap_or(&undef);
    if v.is_undefined() {
        return Err(VmError::type_error(
            "The \"path\" argument must be of type string. Received undefined",
        ));
    }
    value_to_str(v)
}

/// Convert a Value to a String.
/// Convert a Value to a String.
fn value_to_str(v: &Value) -> Result<String, VmError> {
    if let Some(s) = v.as_string() {
        Ok(s.as_str().to_string())
    } else {
        Err(VmError::type_error(
            "The \"path\" argument must be of type string",
        ))
    }
}

/// Normalize a path string (resolve `.` and `..`).
fn normalize_path_string(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let is_absolute = path.starts_with('/') || path.starts_with('\\');

    for part in path.split(|c| c == '/' || c == '\\') {
        match part {
            "" | "." => continue,
            ".." => {
                if !parts.is_empty() && parts.last() != Some(&"..") {
                    parts.pop();
                } else if !is_absolute {
                    parts.push("..");
                }
            }
            _ => parts.push(part),
        }
    }

    let result = parts.join(&MAIN_SEPARATOR.to_string());
    if is_absolute {
        format!("{MAIN_SEPARATOR}{result}")
    } else if result.is_empty() {
        ".".to_string()
    } else {
        result
    }
}
