//! `otter:ffi` — Foreign Function Interface for the Otter runtime.
//!
//! Provides `dlopen`-based dynamic library loading, type marshaling between
//! JS values and C types, direct memory read operations, and CString support.
//!
//! # Usage from JS/TS
//!
//! ```ts
//! import { dlopen, FFIType, suffix } from "otter:ffi";
//!
//! const lib = dlopen(`libsqlite3.${suffix}`, {
//!     sqlite3_libversion: { args: [], returns: "cstring" },
//! });
//! console.log(lib.symbols.sqlite3_libversion()); // "3.45.0"
//! lib.close();
//! ```
//!
//! Requires `--allow-ffi` permission flag.

#![allow(clippy::type_complexity)]
#![allow(clippy::missing_safety_doc)]

pub mod call;
pub mod error;
pub mod extension;
pub mod library;
pub mod pointer;
pub mod types;

pub use error::FfiError;
pub use extension::otter_ffi_extension;
pub use library::{BoundSymbol, FfiLibrary};
pub use pointer::platform_suffix;
pub use types::{FFIType, FfiSignature};
