//! `otter:ffi` native library metadata.
//!
//! FFI is capability-gated at the Rust boundary. This active slice supports
//! type/signature parsing and permission-checked library loading metadata. It
//! intentionally does not store VM callbacks or values in native state.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use libloading::Library;
use otter_runtime::{CapabilitySet, HostedModuleCtx, HostedNativeCall};
use otter_runtime::{
    RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError,
    RuntimeObjectBuilder as ObjectBuilder, RuntimeValue as Value,
};

/// Errors produced by `otter:ffi`.
#[derive(Debug, thiserror::Error)]
pub enum FfiError {
    /// FFI permission denied.
    #[error("ffi permission denied for `{path}`")]
    PermissionDenied {
        /// Path that was rejected.
        path: PathBuf,
    },
    /// Library load failed.
    #[error("failed to load library `{path}`: {reason}")]
    LibraryLoad {
        /// Library path.
        path: PathBuf,
        /// Loader error.
        reason: String,
    },
    /// Unknown FFI type.
    #[error("invalid FFI type `{0}`")]
    InvalidType(String),
}

/// Result alias for `otter:ffi`.
pub type FfiResult<T> = Result<T, FfiError>;

/// Supported FFI scalar and pointer types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum FfiType {
    /// C `int8_t`.
    I8,
    /// C `uint8_t`.
    U8,
    /// C `int16_t`.
    I16,
    /// C `uint16_t`.
    U16,
    /// C `int32_t`.
    I32,
    /// C `uint32_t`.
    U32,
    /// C `int64_t`.
    I64,
    /// C `uint64_t`.
    U64,
    /// C `float`.
    F32,
    /// C `double`.
    F64,
    /// Boolean represented as an unsigned byte.
    Bool,
    /// Raw pointer.
    Ptr,
    /// Null-terminated C string pointer.
    CString,
    /// Void return.
    Void,
}

impl FfiType {
    /// Parse a user-facing type name.
    pub fn parse(name: &str) -> FfiResult<Self> {
        match name {
            "i8" | "int8_t" => Ok(Self::I8),
            "u8" | "uint8_t" => Ok(Self::U8),
            "i16" | "int16_t" => Ok(Self::I16),
            "u16" | "uint16_t" => Ok(Self::U16),
            "i32" | "int32_t" | "int" => Ok(Self::I32),
            "u32" | "uint32_t" => Ok(Self::U32),
            "i64" | "int64_t" => Ok(Self::I64),
            "u64" | "uint64_t" | "usize" => Ok(Self::U64),
            "f32" | "float" => Ok(Self::F32),
            "f64" | "double" => Ok(Self::F64),
            "bool" => Ok(Self::Bool),
            "ptr" | "pointer" => Ok(Self::Ptr),
            "cstring" | "string" => Ok(Self::CString),
            "void" => Ok(Self::Void),
            other => Err(FfiError::InvalidType(other.to_string())),
        }
    }

    /// Canonical type name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::U8 => "u8",
            Self::I16 => "i16",
            Self::U16 => "u16",
            Self::I32 => "i32",
            Self::U32 => "u32",
            Self::I64 => "i64",
            Self::U64 => "u64",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::Bool => "bool",
            Self::Ptr => "ptr",
            Self::CString => "cstring",
            Self::Void => "void",
        }
    }
}

/// Native symbol signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfiSignature {
    /// Argument types.
    pub args: Vec<FfiType>,
    /// Return type.
    pub returns: FfiType,
}

impl FfiSignature {
    /// Parse a signature from user-facing type names.
    pub fn parse(args: &[&str], returns: &str) -> FfiResult<Self> {
        Ok(Self {
            args: args
                .iter()
                .map(|arg| FfiType::parse(arg))
                .collect::<FfiResult<Vec<_>>>()?,
            returns: FfiType::parse(returns)?,
        })
    }
}

/// Permission-checked native library handle.
#[derive(Debug, Clone)]
pub struct FfiLibrary {
    path: PathBuf,
    signatures: BTreeMap<String, FfiSignature>,
    library: Arc<Library>,
}

impl FfiLibrary {
    /// Load a dynamic library after checking `capabilities.ffi`.
    ///
    /// # Safety
    /// Loading a native library can run platform loader hooks and exposes
    /// process-native code. Callers must grant `ffi` deliberately and should
    /// only pass trusted library paths.
    pub unsafe fn open(
        path: impl AsRef<Path>,
        signatures: BTreeMap<String, FfiSignature>,
        capabilities: &CapabilitySet,
    ) -> FfiResult<Self> {
        let path = path.as_ref().to_path_buf();
        if !capabilities.ffi.matches_path(&path) {
            return Err(FfiError::PermissionDenied { path });
        }
        let library = unsafe { Library::new(&path) }.map_err(|err| FfiError::LibraryLoad {
            path: path.clone(),
            reason: err.to_string(),
        })?;
        Ok(Self {
            path,
            signatures,
            library: Arc::new(library),
        })
    }

    /// Path used to open this library.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Known symbol signature.
    #[must_use]
    pub fn signature(&self, name: &str) -> Option<&FfiSignature> {
        self.signatures.get(name)
    }

    /// Keep the loaded library reachable for the lifetime of bound metadata.
    #[must_use]
    pub fn library(&self) -> &Arc<Library> {
        &self.library
    }
}

/// Install the `otter:ffi` namespace object.
pub fn install_ffi_module(ctx: &mut HostedModuleCtx<'_>) -> Result<(), String> {
    let caps = ctx.capabilities().clone();
    let dlopen = std::sync::Arc::new(
        move |ctx: &mut NativeCtx<'_>, args: &[Value], _captures: &[Value]| {
            open_library(ctx, args, &caps)
        },
    );
    ctx.method("dlopen", 1, HostedNativeCall::dynamic(dlopen))?;
    Ok(())
}

fn open_library(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    capabilities: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = crate::arg_string(args, 0, "dlopen")?;
    if path.is_empty() {
        return Err(crate::type_error("dlopen", "library path is required"));
    }
    let signatures = BTreeMap::new();
    let library = unsafe { FfiLibrary::open(&path, signatures, capabilities) }
        .map_err(|err| crate::type_error("dlopen", err.to_string()))?;
    let path_value = crate::string_value(ctx, &library.path().display().to_string())?;
    let mut builder = ObjectBuilder::new(ctx)?;
    builder
        .data_property("path", path_value)
        .map_err(|err| crate::type_error("dlopen", err.to_string()))?;
    let object = builder.build();
    Ok(Value::Object(object))
}
