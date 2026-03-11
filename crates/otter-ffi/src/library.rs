//! Dynamic library loading — cross-platform dlopen/dlsym wrapper.

use std::collections::HashMap;

use libloading::Library as DynLib;

use crate::call::ffi_call;
use crate::error::FfiError;
use crate::types::FfiSignature;

/// A loaded dynamic library with bound symbols.
#[derive(Debug)]
pub struct FfiLibrary {
    /// The underlying dynamic library handle. Kept alive so symbols remain valid.
    _lib: DynLib,
    /// Bound symbols: name -> (function pointer, signature).
    symbols: HashMap<String, BoundSymbol>,
}

/// A resolved symbol: function pointer + its calling signature.
#[derive(Debug)]
pub struct BoundSymbol {
    /// Raw function pointer obtained from dlsym.
    pub ptr: *const (),
    /// The C function signature (argument types + return type).
    pub signature: FfiSignature,
}

// Safety: FfiLibrary is only used from a single-threaded JS VM.
// The function pointers themselves are safe to send across threads.
unsafe impl Send for FfiLibrary {}
unsafe impl Sync for FfiLibrary {}
unsafe impl Send for BoundSymbol {}
unsafe impl Sync for BoundSymbol {}

impl FfiLibrary {
    /// Open a shared library and bind the specified symbols.
    ///
    /// Returns an error if the library cannot be loaded or any symbol is missing.
    pub fn open(path: &str, signatures: &HashMap<String, FfiSignature>) -> Result<Self, FfiError> {
        let lib = unsafe { DynLib::new(path) }.map_err(|e| FfiError::LibraryLoad {
            path: path.to_string(),
            reason: e.to_string(),
        })?;

        let mut symbols = HashMap::with_capacity(signatures.len());
        for (name, sig) in signatures {
            let ptr: *const () = unsafe {
                let sym: libloading::Symbol<*const ()> =
                    lib.get(name.as_bytes())
                        .map_err(|e| FfiError::SymbolNotFound {
                            name: name.clone(),
                            reason: e.to_string(),
                        })?;
                *sym
            };
            symbols.insert(
                name.clone(),
                BoundSymbol {
                    ptr,
                    signature: sig.clone(),
                },
            );
        }

        Ok(FfiLibrary { _lib: lib, symbols })
    }

    /// Get a bound symbol by name.
    pub fn symbol(&self, name: &str) -> Option<&BoundSymbol> {
        self.symbols.get(name)
    }

    /// Get all symbol names.
    pub fn symbol_names(&self) -> impl Iterator<Item = &str> {
        self.symbols.keys().map(String::as_str)
    }

    /// Call a bound symbol with the given raw argument values.
    ///
    /// `args` must be pre-marshaled to the correct C types as raw bytes.
    /// Returns the raw return value as bytes.
    pub fn call_raw(&self, name: &str, arg_values: &[u64]) -> Result<u64, FfiError> {
        let sym = self
            .symbols
            .get(name)
            .ok_or_else(|| FfiError::SymbolNotFound {
                name: name.to_string(),
                reason: "not bound in this library".to_string(),
            })?;

        unsafe { ffi_call(sym.ptr, &sym.signature, arg_values) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_nonexistent_library() {
        let result = FfiLibrary::open("libnonexistent_12345.so", &HashMap::new());
        assert!(result.is_err());
        match result.unwrap_err() {
            FfiError::LibraryLoad { path, .. } => {
                assert_eq!(path, "libnonexistent_12345.so");
            }
            other => panic!("Expected LibraryLoad, got {:?}", other),
        }
    }
}
