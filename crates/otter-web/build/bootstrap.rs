//! Current-codec producer for the one Web bootstrap script.
//!
//! # Contents
//! - The sole ordered source/name declaration and bounded concatenation.
//! - Current frontend compilation, bytecode verification and artifact emission.
//!
//! # Invariants
//! All rows share one classic-script scope. The output has no target addresses,
//! cache keys or format version, and producer failure stops the build.
//!
//! # See also
//! - `otter_bytecode::binary` for the current bounded codec.
//! - `otter_runtime::ExtensionJs` for the static source/code owner.

use std::path::Path;

const MAX_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_BYTECODE_BYTES: usize = 4 * 1024 * 1024;
const SPECIFIER: &str = "<bootstrap:web>";

struct Source {
    path: &'static str,
    text: &'static str,
    defines: &'static [&'static str],
}

const SOURCES: &[Source] = &[
    Source {
        path: "src/web_bootstrap.js",
        text: include_str!("../src/web_bootstrap.js"),
        defines: &[
            "AbortController",
            "AbortSignal",
            "BroadcastChannel",
            "CloseEvent",
            "CustomEvent",
            "DOMException",
            "ErrorEvent",
            "Event",
            "EventTarget",
            "FormData",
            "MessageChannel",
            "MessageEvent",
            "MessagePort",
            "Navigator",
            "performance",
            "Performance",
            "ProgressEvent",
            "PromiseRejectionEvent",
            "reportError",
            "TextDecoder",
            "TextEncoder",
            "URLSearchParams",
        ],
    },
    Source {
        path: "src/web_streams.js",
        text: include_str!("../src/web_streams.js"),
        defines: &[
            "ByteLengthQueuingStrategy",
            "CompressionStream",
            "CountQueuingStrategy",
            "DecompressionStream",
            "ReadableStream",
            "ReadableByteStreamController",
            "ReadableStreamBYOBReader",
            "ReadableStreamBYOBRequest",
            "ReadableStreamDefaultController",
            "ReadableStreamDefaultReader",
            "TextDecoderStream",
            "TextEncoderStream",
            "TransformStream",
            "TransformStreamDefaultController",
            "WritableStream",
            "WritableStreamDefaultController",
            "WritableStreamDefaultWriter",
        ],
    },
    Source {
        path: "src/web_fetch.js",
        text: include_str!("../src/web_fetch.js"),
        defines: &["fetch", "Headers", "Request", "Response"],
    },
    Source {
        path: "src/web_urlpattern.js",
        text: include_str!("../src/web_urlpattern.js"),
        defines: &["URLPattern"],
    },
    Source {
        path: "src/web_console.js",
        text: include_str!("../src/web_console.js"),
        defines: &["Console"],
    },
];

fn assemble(sources: &[Source], max_bytes: usize) -> Result<String, std::io::Error> {
    let mut text = String::new();
    for source in sources {
        let additional = source
            .text
            .len()
            .checked_add(3)
            .ok_or_else(|| std::io::Error::other("Web bootstrap source length overflow"))?;
        let length = text
            .len()
            .checked_add(additional)
            .ok_or_else(|| std::io::Error::other("Web bootstrap source length overflow"))?;
        if length > max_bytes {
            return Err(std::io::Error::other(
                "Web bootstrap source exceeds build budget",
            ));
        }
        text.try_reserve_exact(additional)
            .map_err(std::io::Error::other)?;
        text.push_str(source.text);
        text.push_str("\n;\n");
    }
    Ok(text)
}

pub(crate) fn generate(out: &Path) -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build/bootstrap.rs");
    for source in SOURCES {
        println!("cargo:rerun-if-changed={}", source.path);
    }
    let text = assemble(SOURCES, MAX_SOURCE_BYTES)?;
    let compiled = otter_compiler::compile_script_source_to_module(
        &text,
        otter_syntax::SourceKind::JavaScript,
        SPECIFIER,
    )?;
    let encoded =
        otter_bytecode::binary::encode_module_bounded(&compiled.bytecode, MAX_BYTECODE_BYTES)?;
    otter_bytecode::binary::decode_module(&encoded)?;
    let names = SOURCES
        .iter()
        .flat_map(|source| source.defines.iter())
        .map(|name| format!("{name:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let descriptor = format!(
        r#"::otter_runtime::ExtensionJs {{
    source: include_str!(concat!(env!("OUT_DIR"), "/web-bootstrap.js")),
    bytecode: include_bytes!(concat!(env!("OUT_DIR"), "/web-bootstrap.bytecode")),
    defines: &[{names}],
}}
"#
    );
    std::fs::write(out.join("web-bootstrap.js"), text)?;
    std::fs::write(out.join("web-bootstrap.bytecode"), encoded)?;
    std::fs::write(out.join("web-bootstrap.rs"), descriptor)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_shared_scope_source_and_bounded_producer() {
        let text = assemble(SOURCES, MAX_SOURCE_BYTES).expect("bounded source");
        assert_eq!(text.len(), 120_590);
        assert!(assemble(SOURCES, text.len() - 1).is_err());
        let out = tempfile::tempdir().expect("producer directory");
        generate(out.path()).expect("actual bounded producer");
        let asset = crate::WEB_EXTENSION.js.expect("one generated bundle");
        assert_eq!(asset.source, text);
        assert_eq!(
            std::fs::read(out.path().join("web-bootstrap.js")).unwrap(),
            text.as_bytes()
        );
        assert_eq!(
            std::fs::read(out.path().join("web-bootstrap.bytecode")).unwrap(),
            asset.bytecode
        );
        let verified =
            otter_bytecode::binary::decode_module(asset.bytecode).expect("mandatory verifier");
        assert!(matches!(
            otter_bytecode::binary::encode_module_bounded(verified.module(), 1),
            Err(otter_bytecode::binary::ModuleEncodeError::SizeLimitExceeded)
        ));
        assert_eq!(verified.function_base(), 0);
        assert_eq!(verified.module().module, SPECIFIER);
    }
}
