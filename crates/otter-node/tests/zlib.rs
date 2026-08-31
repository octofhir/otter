//! End-to-end coverage for the caller-buffered `node:zlib` native boundary.
//!
//! # Invariants
//! - Every codec can make progress through output chunks much smaller than the
//!   decoded payload without retaining the whole result in its native handle.
//! - The public surface remains Node's existing options and Buffer API.

use otter_node::NodeApiBuilderExt;
use otter_runtime::{CapabilitySet, Runtime};

#[test]
fn node_zlib_round_trips_through_small_output_chunks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = dir.path().join("zlib.cjs");
    std::fs::write(
        &entry,
        r#"
        const zlib = require("node:zlib");
        const source = Buffer.from("otter:hé🦦:streaming\n".repeat(8192));
        const options = { chunkSize: 64 };
        const codecs = [
          ["gzip", zlib.gzipSync, zlib.gunzipSync],
          ["deflate", zlib.deflateSync, zlib.inflateSync],
          ["brotli", zlib.brotliCompressSync, zlib.brotliDecompressSync],
          ["zstd", zlib.zstdCompressSync, zlib.zstdDecompressSync],
        ];

        for (const [name, encode, decode] of codecs) {
          const compressed = encode(source, options);
          const restored = decode(compressed, options);
          if (!restored.equals(source)) {
            throw new Error(`${name} restored ${restored.length}/${source.length} bytes`);
          }
        }

        const dictionary = Buffer.from("otter-dictionary-".repeat(1024));
        const compressed = zlib.deflateSync(source, { ...options, dictionary });
        const restored = zlib.inflateSync(compressed, { ...options, dictionary });
        if (!restored.equals(source)) throw new Error("dictionary round-trip failed");
        "#,
    )
    .expect("write zlib fixture");

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .expect("runtime with Node APIs");
    runtime.run_file(&entry).expect("zlib fixture");
}

#[test]
fn native_zlib_parameter_change_delivers_pre_admitted_carry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = dir.path().join("params.cjs");
    std::fs::write(
        &entry,
        r#"
        const native = require("internal/otter/zlib");
        const { constants, inflateSync } = require("node:zlib");
        const source = Buffer.from("parameter-change-carry\n".repeat(8192));
        const chunks = [];
        const id = native.create(1); // DEFLATE
        native.init(id, 15, 6, 8, constants.Z_DEFAULT_STRATEGY, undefined);
        let offset = 0;
        let remaining = source.length;

        function step(flush) {
          const output = Buffer.alloc(64);
          const result = native.process(
            id, flush, source, offset, remaining, output, 0, output.length);
          if (result.error !== undefined) throw new Error(result.error);
          const consumed = remaining - result.availInAfter;
          offset += consumed;
          remaining = result.availInAfter;
          const produced = output.length - result.availOutAfter;
          if (produced !== 0) chunks.push(output.slice(0, produced));
          return result.availOutAfter;
        }

        step(constants.Z_NO_FLUSH);
        native.params(id, 1, constants.Z_HUFFMAN_ONLY);
        let finished = false;
        for (let attempt = 0; attempt < 100000; attempt++) {
          if (step(constants.Z_FINISH) !== 0) {
            finished = true;
            break;
          }
        }
        native.close(id);
        if (!finished || remaining !== 0) throw new Error("deflate did not finish");

        const restored = inflateSync(Buffer.concat(chunks), { chunkSize: 64 });
        if (!restored.equals(source)) {
          throw new Error(`params restored ${restored.length}/${source.length} bytes`);
        }
        "#,
    )
    .expect("write params fixture");

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .expect("runtime with Node APIs");
    runtime.run_file(&entry).expect("zlib params fixture");
}
