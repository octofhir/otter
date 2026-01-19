/**
 * Node.js zlib module implementation for Otter.
 *
 * Provides gzip, deflate, and brotli compression/decompression.
 */
(function (global) {
  "use strict";

  // Constants matching Node.js zlib
  const constants = {
    // Compression levels
    Z_NO_COMPRESSION: 0,
    Z_BEST_SPEED: 1,
    Z_BEST_COMPRESSION: 9,
    Z_DEFAULT_COMPRESSION: -1,

    // Flush modes
    Z_NO_FLUSH: 0,
    Z_PARTIAL_FLUSH: 1,
    Z_SYNC_FLUSH: 2,
    Z_FULL_FLUSH: 3,
    Z_FINISH: 4,
    Z_BLOCK: 5,
    Z_TREES: 6,

    // Strategy
    Z_FILTERED: 1,
    Z_HUFFMAN_ONLY: 2,
    Z_RLE: 3,
    Z_FIXED: 4,
    Z_DEFAULT_STRATEGY: 0,

    // Brotli
    BROTLI_DECODE: 0,
    BROTLI_ENCODE: 1,
    BROTLI_OPERATION_PROCESS: 0,
    BROTLI_OPERATION_FLUSH: 1,
    BROTLI_OPERATION_FINISH: 2,
    BROTLI_PARAM_MODE: 0,
    BROTLI_MODE_GENERIC: 0,
    BROTLI_MODE_TEXT: 1,
    BROTLI_MODE_FONT: 2,
    BROTLI_PARAM_QUALITY: 1,
    BROTLI_MIN_QUALITY: 0,
    BROTLI_MAX_QUALITY: 11,
    BROTLI_DEFAULT_QUALITY: 11,
    BROTLI_PARAM_LGWIN: 2,
    BROTLI_MIN_WINDOW_BITS: 10,
    BROTLI_MAX_WINDOW_BITS: 24,
    BROTLI_DEFAULT_WINDOW: 22,
    BROTLI_PARAM_LGBLOCK: 3,
    BROTLI_MIN_INPUT_BLOCK_BITS: 16,
    BROTLI_MAX_INPUT_BLOCK_BITS: 24,
    BROTLI_PARAM_SIZE_HINT: 5,
  };

  // Helper to ensure Buffer input
  function ensureBuffer(input) {
    if (typeof input === "string") {
      return Buffer.from(input);
    }
    if (Buffer.isBuffer(input)) {
      return input;
    }
    if (input instanceof Uint8Array) {
      return Buffer.from(input);
    }
    if (ArrayBuffer.isView(input)) {
      return Buffer.from(input.buffer, input.byteOffset, input.byteLength);
    }
    if (input instanceof ArrayBuffer) {
      return Buffer.from(input);
    }
    throw new TypeError(
      "The first argument must be of type string or an instance of Buffer, ArrayBuffer, or Array"
    );
  }

  // Helper to create async wrapper from sync function
  function createAsyncWrapper(syncFn) {
    return function (buffer, options, callback) {
      if (typeof options === "function") {
        callback = options;
        options = {};
      }

      // Promise-based if no callback
      if (typeof callback !== "function") {
        return new Promise((resolve, reject) => {
          try {
            const result = syncFn(buffer, options);
            resolve(result);
          } catch (err) {
            reject(err);
          }
        });
      }

      // Callback-based
      try {
        const result = syncFn(buffer, options);
        queueMicrotask(() => callback(null, result));
      } catch (err) {
        queueMicrotask(() => callback(err));
      }
    };
  }

  // ==========================================================================
  // Sync functions
  // ==========================================================================

  function gzipSync(buffer, options) {
    const buf = ensureBuffer(buffer);
    const result = __otter_zlib_gzip_sync(buf, options || {});
    return Buffer.from(result.data);
  }

  function gunzipSync(buffer, options) {
    const buf = ensureBuffer(buffer);
    const result = __otter_zlib_gunzip_sync(buf);
    return Buffer.from(result.data);
  }

  function deflateSync(buffer, options) {
    const buf = ensureBuffer(buffer);
    const result = __otter_zlib_deflate_sync(buf, options || {});
    return Buffer.from(result.data);
  }

  function inflateSync(buffer, options) {
    const buf = ensureBuffer(buffer);
    const result = __otter_zlib_inflate_sync(buf);
    return Buffer.from(result.data);
  }

  function deflateRawSync(buffer, options) {
    const buf = ensureBuffer(buffer);
    const result = __otter_zlib_deflate_raw_sync(buf, options || {});
    return Buffer.from(result.data);
  }

  function inflateRawSync(buffer, options) {
    const buf = ensureBuffer(buffer);
    const result = __otter_zlib_inflate_raw_sync(buf);
    return Buffer.from(result.data);
  }

  function brotliCompressSync(buffer, options) {
    const buf = ensureBuffer(buffer);
    const result = __otter_zlib_brotli_compress_sync(buf, options || {});
    return Buffer.from(result.data);
  }

  function brotliDecompressSync(buffer, options) {
    const buf = ensureBuffer(buffer);
    const result = __otter_zlib_brotli_decompress_sync(buf);
    return Buffer.from(result.data);
  }

  // Aliases
  const unzipSync = gunzipSync;
  const compressSync = deflateSync;
  const uncompressSync = inflateSync;

  // ==========================================================================
  // Async functions
  // ==========================================================================

  const gzip = createAsyncWrapper(gzipSync);
  const gunzip = createAsyncWrapper(gunzipSync);
  const deflate = createAsyncWrapper(deflateSync);
  const inflate = createAsyncWrapper(inflateSync);
  const deflateRaw = createAsyncWrapper(deflateRawSync);
  const inflateRaw = createAsyncWrapper(inflateRawSync);
  const brotliCompress = createAsyncWrapper(brotliCompressSync);
  const brotliDecompress = createAsyncWrapper(brotliDecompressSync);
  const unzip = createAsyncWrapper(unzipSync);
  const compress = deflate;
  const uncompress = inflate;

  // ==========================================================================
  // CRC32
  // ==========================================================================

  function crc32(data, value) {
    const buf = ensureBuffer(data);
    // Note: Node.js crc32 supports continuing from a previous value,
    // but our implementation is one-shot for now
    return __otter_zlib_crc32(buf);
  }

  // ==========================================================================
  // Stream classes (basic implementation)
  // ==========================================================================

  // Base Zlib class extending Transform stream
  class Zlib {
    constructor(options) {
      this._options = options || {};
      this._chunks = [];
    }

    _transform(chunk, encoding, callback) {
      this._chunks.push(ensureBuffer(chunk));
      callback();
    }

    _flush(callback) {
      callback();
    }
  }

  class Gzip extends Zlib {
    constructor(options) {
      super(options);
    }

    _flush(callback) {
      try {
        const combined = Buffer.concat(this._chunks);
        const result = gzipSync(combined, this._options);
        callback(null, result);
      } catch (err) {
        callback(err);
      }
    }
  }

  class Gunzip extends Zlib {
    _flush(callback) {
      try {
        const combined = Buffer.concat(this._chunks);
        const result = gunzipSync(combined);
        callback(null, result);
      } catch (err) {
        callback(err);
      }
    }
  }

  class Deflate extends Zlib {
    constructor(options) {
      super(options);
    }

    _flush(callback) {
      try {
        const combined = Buffer.concat(this._chunks);
        const result = deflateSync(combined, this._options);
        callback(null, result);
      } catch (err) {
        callback(err);
      }
    }
  }

  class Inflate extends Zlib {
    _flush(callback) {
      try {
        const combined = Buffer.concat(this._chunks);
        const result = inflateSync(combined);
        callback(null, result);
      } catch (err) {
        callback(err);
      }
    }
  }

  class DeflateRaw extends Zlib {
    constructor(options) {
      super(options);
    }

    _flush(callback) {
      try {
        const combined = Buffer.concat(this._chunks);
        const result = deflateRawSync(combined, this._options);
        callback(null, result);
      } catch (err) {
        callback(err);
      }
    }
  }

  class InflateRaw extends Zlib {
    _flush(callback) {
      try {
        const combined = Buffer.concat(this._chunks);
        const result = inflateRawSync(combined);
        callback(null, result);
      } catch (err) {
        callback(err);
      }
    }
  }

  class BrotliCompress extends Zlib {
    constructor(options) {
      super(options);
    }

    _flush(callback) {
      try {
        const combined = Buffer.concat(this._chunks);
        const result = brotliCompressSync(combined, this._options);
        callback(null, result);
      } catch (err) {
        callback(err);
      }
    }
  }

  class BrotliDecompress extends Zlib {
    _flush(callback) {
      try {
        const combined = Buffer.concat(this._chunks);
        const result = brotliDecompressSync(combined);
        callback(null, result);
      } catch (err) {
        callback(err);
      }
    }
  }

  class Unzip extends Zlib {
    _flush(callback) {
      try {
        const combined = Buffer.concat(this._chunks);
        // Try gunzip first, fall back to inflate
        try {
          const result = gunzipSync(combined);
          callback(null, result);
        } catch {
          const result = inflateSync(combined);
          callback(null, result);
        }
      } catch (err) {
        callback(err);
      }
    }
  }

  // Factory functions
  function createGzip(options) {
    return new Gzip(options);
  }

  function createGunzip(options) {
    return new Gunzip(options);
  }

  function createDeflate(options) {
    return new Deflate(options);
  }

  function createInflate(options) {
    return new Inflate(options);
  }

  function createDeflateRaw(options) {
    return new DeflateRaw(options);
  }

  function createInflateRaw(options) {
    return new InflateRaw(options);
  }

  function createBrotliCompress(options) {
    return new BrotliCompress(options);
  }

  function createBrotliDecompress(options) {
    return new BrotliDecompress(options);
  }

  function createUnzip(options) {
    return new Unzip(options);
  }

  // ==========================================================================
  // Module exports
  // ==========================================================================

  const zlib = {
    // Constants
    constants,
    ...constants,

    // Sync functions
    gzipSync,
    gunzipSync,
    deflateSync,
    inflateSync,
    deflateRawSync,
    inflateRawSync,
    brotliCompressSync,
    brotliDecompressSync,
    unzipSync,
    compressSync,
    uncompressSync,

    // Async functions
    gzip,
    gunzip,
    deflate,
    inflate,
    deflateRaw,
    inflateRaw,
    brotliCompress,
    brotliDecompress,
    unzip,
    compress,
    uncompress,

    // CRC32
    crc32,

    // Stream classes
    Gzip,
    Gunzip,
    Deflate,
    Inflate,
    DeflateRaw,
    InflateRaw,
    BrotliCompress,
    BrotliDecompress,
    Unzip,
    Zlib,

    // Factory functions
    createGzip,
    createGunzip,
    createDeflate,
    createInflate,
    createDeflateRaw,
    createInflateRaw,
    createBrotliCompress,
    createBrotliDecompress,
    createUnzip,
  };

  // Register as node:zlib module
  if (typeof __registerNodeBuiltin === "function") {
    __registerNodeBuiltin("zlib", zlib);
  }

  // Also expose on global for direct access
  global.__otter_zlib = zlib;
})(globalThis);
