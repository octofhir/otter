//! Streaming half of `node:zlib`: the deflate and inflate streams the
//! vendored `zlib.js` drives through `internalBinding('zlib')`.
//!
//! # Contents
//! - [`zlib_stream_binding_cjs_value`] exports the create/init/process
//!   surface as `internal/otter/zlib`, which the compat `Zlib` handle class
//!   wraps in the shape Node's own binding has.
//! - A table of live streams owned by the natives, keyed by the id the
//!   handle holds.
//!
//! # Invariants
//! - One `z_stream` per handle, driven exactly the way zlib is driven: the
//!   caller owns both buffers, and every call reports what is left of each,
//!   which is the contract `_processChunk` loops on.
//! - The stream's own state — window bits, memory level, strategy, level —
//!   is set once at `init` and changed only through `params`, so a live
//!   parameter change keeps the stream unbroken.
//!
//! # See also
//! - `nodelib/compat/internal_zlib_handle.js`

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use libz_rs_sys as z;
use otter_runtime::{
    CapabilitySet, RuntimeLocal, RuntimeNativeCtx, RuntimeNativeError, RuntimeNativeScope,
    RuntimeTaskSpawner, RuntimeValue, runtime_type_error,
};

/// `node_zlib_mode` — the stream a handle drives.
const DEFLATE: u32 = 1;
const GZIP: u32 = 3;
const GUNZIP: u32 = 4;
const DEFLATE_RAW: u32 = 5;
const INFLATE_RAW: u32 = 6;
const UNZIP: u32 = 7;
const BROTLI_DECODE: u32 = 8;
const BROTLI_ENCODE: u32 = 9;
const ZSTD_COMPRESS: u32 = 10;
const ZSTD_DECOMPRESS: u32 = 11;

/// Brotli's operations, as the JS side passes them through in place of a
/// zlib flush value.
const BROTLI_OPERATION_PROCESS: i32 = 0;
const BROTLI_OPERATION_FLUSH: i32 = 1;
const BROTLI_OPERATION_FINISH: i32 = 2;

/// Zstd's end directives, likewise.
const ZSTD_E_CONTINUE: i32 = 0;
const ZSTD_E_FLUSH: i32 = 1;
const ZSTD_E_END: i32 = 2;

/// A `z_stream` and the buffers it points at, owned by one handle.
///
/// The struct holds raw pointers, which is what makes it `!Send` by
/// default. A stream is reachable only through the handle table's mutex
/// and is driven from the isolate thread that created it, so the pointers
/// are never shared across threads.
struct OwnedStream(Box<z::z_stream>);

// SAFETY: see the type's documentation — ownership is single-threaded and
// serialized by the table's mutex.
unsafe impl Send for OwnedStream {}

/// The codecs `node:zlib` streams besides deflate. Each writes into a
/// buffer the handle drains into the caller's output, which is what makes
/// them behave like the zlib stream next to them.
enum Codec {
    BrotliEncode(Box<brotli::CompressorWriter<Vec<u8>>>),
    BrotliDecode(Box<brotli::DecompressorWriter<Vec<u8>>>),
    ZstdCompress(Box<zstd_safe::CCtx<'static>>),
    ZstdDecompress(Box<zstd_safe::DCtx<'static>>),
}

// SAFETY: as with the zlib stream, a codec is reachable only through the
// handle table's mutex and is driven from the isolate thread.
unsafe impl Send for Codec {}

/// A live stream and everything needed to restart it.
struct Handle {
    mode: u32,
    stream: OwnedStream,
    initialized: bool,
    dictionary: Vec<u8>,
    window_bits: i32,
    /// Bytes the stream produced outside a `process` call — a parameter
    /// change closes the open block — waiting for the caller's next buffer.
    carry: Vec<u8>,
    /// Non-deflate codec, when the mode names one.
    codec: Option<Codec>,
    /// Set once the stream reported `Z_STREAM_END`, so a further call
    /// answers "no progress" instead of re-running a finished stream.
    finished: bool,
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.initialized || is_codec(self.mode) {
            return;
        }
        // SAFETY: the stream was initialized by the matching init call and
        // is ended exactly once, here.
        unsafe {
            if is_deflate(self.mode) {
                z::deflateEnd(&raw mut *self.stream.0);
            } else {
                z::inflateEnd(&raw mut *self.stream.0);
            }
        }
    }
}

const fn is_deflate(mode: u32) -> bool {
    matches!(mode, DEFLATE | DEFLATE_RAW | GZIP)
}

/// Whether the mode is one of the codecs that is not zlib.
const fn is_codec(mode: u32) -> bool {
    matches!(
        mode,
        BROTLI_ENCODE | BROTLI_DECODE | ZSTD_COMPRESS | ZSTD_DECOMPRESS
    )
}

/// The `windowBits` value zlib wants for a mode: negative for a raw
/// stream, `+16` for gzip framing, `+32` for "detect zlib or gzip".
fn window_bits_for(mode: u32, bits: i32) -> i32 {
    match mode {
        // `unzip` reads the framing from the stream itself.
        UNZIP => 15 + 32,
        DEFLATE | DEFLATE_RAW | GZIP => {
            // zlib's smallest deflate window is 9 bits; 8 asks for the
            // same stream, which is what zlib substitutes.
            let bits = bits.clamp(9, 15);
            if mode == DEFLATE_RAW {
                -bits
            } else if mode == GZIP {
                bits + 16
            } else {
                bits
            }
        }
        _ => {
            // Inflate takes 0 as "use the window the header names", which
            // is what a caller that named no window bits wants.
            if bits == 0 {
                return if mode == GUNZIP { 15 + 16 } else { 0 };
            }
            // As with deflate, 8 names the same window zlib inflates
            // with at 9.
            let bits = bits.clamp(9, 15);
            if mode == INFLATE_RAW {
                -bits
            } else if mode == GUNZIP {
                bits + 16
            } else {
                bits
            }
        }
    }
}

type Table = Arc<Mutex<HashMap<u32, Handle>>>;

/// Build the CommonJS export of `internal/otter/zlib`.
///
/// # Errors
/// Returns a native error when the surface fails to allocate.
pub fn zlib_stream_binding_cjs_value<'scope>(
    scope: &mut RuntimeNativeScope<'scope, '_>,
    _capabilities: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    _module: RuntimeLocal<'scope>,
    _require: RuntimeLocal<'scope>,
) -> Result<RuntimeLocal<'scope>, RuntimeNativeError> {
    let object = scope.object()?;
    let table: Table = Arc::new(Mutex::new(HashMap::new()));
    let next_id = Arc::new(std::sync::atomic::AtomicU32::new(1));

    let create_table = table.clone();
    let create_ids = next_id.clone();
    let create = scope.native_closure(
        "create",
        1,
        &[],
        move |_ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            let mode = args.first().and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
            let id = create_ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            create_table
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(
                    id,
                    Handle {
                        mode,
                        stream: OwnedStream(Box::new(unsafe { std::mem::zeroed() })),
                        initialized: false,
                        dictionary: Vec::new(),
                        window_bits: 15,
                        carry: Vec::new(),
                        codec: None,
                        finished: false,
                    },
                );
            Ok(RuntimeValue::number_u32(id))
        },
    )?;
    scope.set(object, "create", create)?;

    let init_table = table.clone();
    let init = scope.native_closure(
        "init",
        6,
        &[],
        move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            init_stream(ctx, args, &init_table)
        },
    )?;
    scope.set(object, "init", init)?;

    let process_table = table.clone();
    let process = scope.native_closure(
        "process",
        8,
        &[],
        move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            process_chunk(ctx, args, &process_table)
        },
    )?;
    scope.set(object, "process", process)?;

    let reset_table = table.clone();
    let reset = scope.native_closure(
        "reset",
        1,
        &[],
        move |_ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            let id = args.first().and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
            let mut table = reset_table
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(handle) = table.get_mut(&id) else {
                return Err(runtime_type_error(
                    "zlib.reset",
                    "unknown handle".to_string(),
                ));
            };
            if handle.initialized {
                let stream = &raw mut *handle.stream.0;
                // SAFETY: the stream is live for as long as the handle is.
                let status = unsafe {
                    if is_deflate(handle.mode) {
                        z::deflateReset(stream)
                    } else {
                        z::inflateReset(stream)
                    }
                };
                if status != z::Z_OK {
                    return Err(runtime_type_error(
                        "zlib.reset",
                        format!("reset failed with {status}"),
                    ));
                }
                handle.finished = false;
                install_dictionary(handle);
            }
            Ok(RuntimeValue::undefined())
        },
    )?;
    scope.set(object, "reset", reset)?;

    let params_table = table.clone();
    let params = scope.native_closure(
        "params",
        3,
        &[],
        move |_ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            let id = args.first().and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
            let level = args.get(1).and_then(|v| v.as_f64()).unwrap_or(-1.0) as i32;
            let strategy = args.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0) as i32;
            let mut table = params_table
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(handle) = table.get_mut(&id) else {
                return Err(runtime_type_error(
                    "zlib.params",
                    "unknown handle".to_string(),
                ));
            };
            if !handle.initialized || !is_deflate(handle.mode) {
                return Ok(RuntimeValue::undefined());
            }
            // zlib closes the open block before the new parameters take
            // effect, so it needs room to write it; the caller has no
            // buffer here and the bytes belong to the stream, so they go
            // into the stream's own scratch and out with the next call.
            let mut scratch = vec![0u8; 64 * 1024];
            let stream = &raw mut *handle.stream.0;
            // SAFETY: `scratch` outlives the call, and the stream is live.
            let status = unsafe {
                (*stream).next_out = scratch.as_mut_ptr();
                (*stream).avail_out = scratch.len() as u32;
                (*stream).next_in = std::ptr::null_mut();
                (*stream).avail_in = 0;
                let status = z::deflateParams(stream, level, strategy);
                let produced = scratch.len() - (*stream).avail_out as usize;
                scratch.truncate(produced);
                (*stream).next_out = std::ptr::null_mut();
                (*stream).avail_out = 0;
                status
            };
            if status != z::Z_OK && status != z::Z_BUF_ERROR {
                return Err(runtime_type_error(
                    "zlib.params",
                    format!("deflateParams failed with {status}"),
                ));
            }
            handle.dictionary.shrink_to_fit();
            handle.carry_extend(scratch);
            Ok(RuntimeValue::undefined())
        },
    )?;
    scope.set(object, "params", params)?;

    let close_table = table.clone();
    let close = scope.native_closure(
        "close",
        1,
        &[],
        move |_ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            let id = args.first().and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
            close_table
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            Ok(RuntimeValue::undefined())
        },
    )?;
    scope.set(object, "close", close)?;

    let crc = scope.native_closure(
        "crc32",
        2,
        &[],
        move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
            let seed = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
            // `zlib.crc32` takes a string as readily as a view; a string is
            // hashed as its UTF-8 bytes, the way Node hashes it.
            let bytes = match args
                .first()
                .copied()
                .and_then(|value| buffer_source_bytes(ctx, value))
            {
                Some(bytes) => bytes,
                None => otter_runtime::runtime_arg_to_string(args, 0, ctx.heap()).into_bytes(),
            };
            let mut hasher = crc32fast::Hasher::new_with_initial(seed);
            hasher.update(&bytes);
            Ok(RuntimeValue::number_u32(hasher.finalize()))
        },
    )?;
    scope.set(object, "crc32", crc)?;

    Ok(object)
}

impl Handle {
    /// Park bytes the stream produced outside a `process` call.
    fn carry_extend(&mut self, bytes: Vec<u8>) {
        self.carry.extend(bytes);
    }
}

/// Bytes of an input argument, whatever kind of buffer source it is.
///
/// `zlib` takes a string, a Buffer, any typed array, a `DataView` or a
/// bare `ArrayBuffer`, so the binding reads all of them.
fn buffer_source_bytes(ctx: &mut RuntimeNativeCtx<'_>, value: RuntimeValue) -> Option<Vec<u8>> {
    if let Some(view) = value.as_typed_array(ctx.heap()) {
        let heap = ctx.heap();
        let offset = view.byte_offset(heap);
        let length = view.byte_length(heap);
        return Some(
            view.buffer(heap)
                .with_bytes(heap, |bytes| bytes[offset..offset + length].to_vec()),
        );
    }
    if let Some(view) = value.as_data_view() {
        let heap = ctx.heap();
        let offset = view.byte_offset(heap);
        let length = view.byte_length(heap);
        return Some(
            view.buffer(heap)
                .with_bytes(heap, |bytes| bytes[offset..offset + length].to_vec()),
        );
    }
    if let Some(buffer) = value.as_array_buffer() {
        let heap = ctx.heap();
        return Some(buffer.with_bytes(heap, <[u8]>::to_vec));
    }
    None
}

/// Install the handle's dictionary where zlib takes one up front.
///
/// Deflate always takes it at the start. A raw inflate has no header to
/// announce a dictionary with, so it takes one before the first byte too;
/// every other inflate waits for zlib to ask (`Z_NEED_DICT`).
fn install_dictionary(handle: &mut Handle) {
    if handle.dictionary.is_empty() {
        return;
    }
    let deflating = is_deflate(handle.mode);
    if !deflating && handle.mode != INFLATE_RAW {
        return;
    }
    let stream = &raw mut *handle.stream.0;
    let pointer = handle.dictionary.as_ptr();
    let length = handle.dictionary.len() as u32;
    // SAFETY: the stream is live and the dictionary outlives the call.
    unsafe {
        if deflating {
            z::deflateSetDictionary(stream, pointer, length);
        } else {
            z::inflateSetDictionary(stream, pointer, length);
        }
    }
}

/// Start the stream for the handle's mode.
fn init_stream(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let id = args.first().and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
    let window_bits = args.get(1).and_then(|v| v.as_f64()).unwrap_or(15.0) as i32;
    let level = args.get(2).and_then(|v| v.as_f64()).unwrap_or(-1.0) as i32;
    let mem_level = args.get(3).and_then(|v| v.as_f64()).unwrap_or(8.0) as i32;
    let strategy = args.get(4).and_then(|v| v.as_f64()).unwrap_or(0.0) as i32;
    let dictionary = args
        .get(5)
        .copied()
        .and_then(|value| buffer_source_bytes(ctx, value))
        .unwrap_or_default();

    let mut table = table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(handle) = table.get_mut(&id) else {
        return Err(runtime_type_error(
            "zlib.init",
            "unknown handle".to_string(),
        ));
    };
    if is_codec(handle.mode) {
        // Brotli takes its quality and window from the parameter array the
        // JS side passes as `level`/`windowBits`; zstd takes its level.
        handle.codec = Some(match handle.mode {
            BROTLI_ENCODE => Codec::BrotliEncode(Box::new(brotli::CompressorWriter::new(
                Vec::new(),
                4096,
                if level < 0 {
                    11
                } else {
                    (level as u32).min(11)
                },
                if window_bits <= 0 {
                    22
                } else {
                    (window_bits as u32).clamp(10, 24)
                },
            ))),
            BROTLI_DECODE => {
                Codec::BrotliDecode(Box::new(brotli::DecompressorWriter::new(Vec::new(), 4096)))
            }
            ZSTD_COMPRESS => {
                let mut context = zstd_safe::CCtx::create();
                if level > 0 {
                    let _ = context.set_parameter(zstd_safe::CParameter::CompressionLevel(level));
                }
                Codec::ZstdCompress(Box::new(context))
            }
            _ => Codec::ZstdDecompress(Box::new(zstd_safe::DCtx::create())),
        });
        handle.initialized = true;
        handle.finished = false;
        return Ok(RuntimeValue::undefined());
    }
    let bits = window_bits_for(handle.mode, window_bits);
    handle.window_bits = bits;
    handle.dictionary = dictionary;
    let stream = &raw mut *handle.stream.0;
    let version = c"1.3.1".as_ptr();
    let stream_size = std::mem::size_of::<z::z_stream>() as i32;
    // SAFETY: the stream is a live, zeroed `z_stream` this handle owns; the
    // matching `End` runs in `Drop`.
    let status = unsafe {
        if is_deflate(handle.mode) {
            z::deflateInit2_(
                stream,
                level,
                z::Z_DEFLATED,
                bits,
                mem_level,
                strategy,
                version,
                stream_size,
            )
        } else {
            z::inflateInit2_(stream, bits, version, stream_size)
        }
    };
    if status != z::Z_OK {
        return Err(runtime_type_error(
            "zlib.init",
            format!("stream init failed with {status}"),
        ));
    }
    handle.initialized = true;
    handle.finished = false;
    install_dictionary(handle);
    Ok(RuntimeValue::undefined())
}

/// Move input through the stream into the caller's output buffer and
/// report what is left of each side.
fn process_chunk(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let id = args.first().and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
    let flush = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0) as i32;
    let in_off = args.get(3).and_then(|v| v.as_f64()).unwrap_or(0.0) as usize;
    let in_len = args.get(4).and_then(|v| v.as_f64()).unwrap_or(0.0) as usize;
    let out_off = args.get(6).and_then(|v| v.as_f64()).unwrap_or(0.0) as usize;
    let out_len = args.get(7).and_then(|v| v.as_f64()).unwrap_or(0.0) as usize;

    let input = args
        .get(2)
        .copied()
        .and_then(|value| buffer_source_bytes(ctx, value))
        .map(|bytes| {
            let start = in_off.min(bytes.len());
            let end = (start + in_len).min(bytes.len());
            bytes[start..end].to_vec()
        })
        .unwrap_or_default();

    let mut output = vec![0u8; out_len];
    let outcome = {
        let mut table = table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(handle) = table.get_mut(&id) else {
            return Err(runtime_type_error(
                "zlib.process",
                "unknown handle".to_string(),
            ));
        };
        run_stream(handle, flush, &input, &mut output)
    };

    let (consumed, produced, error) = match outcome {
        Ok(pair) => (pair.0, pair.1, None),
        Err(message) => (0, 0, Some(message)),
    };

    if produced > 0
        && let Some(view) = args.get(5).and_then(|v| v.as_typed_array(ctx.heap()))
    {
        let heap = ctx.heap_mut();
        let base = view.byte_offset(heap);
        view.buffer(heap).with_bytes_mut(heap, |bytes| {
            let start = base + out_off;
            let end = (start + produced).min(bytes.len());
            if start < end {
                bytes[start..end].copy_from_slice(&output[..end - start]);
            }
        });
    }

    ctx.scope(|mut scope| {
        let result = scope.object()?;
        let avail_out = scope.number((out_len - produced) as f64);
        scope.set(result, "availOutAfter", avail_out)?;
        let avail_in = scope.number(in_len.saturating_sub(consumed) as f64);
        scope.set(result, "availInAfter", avail_in)?;
        if let Some(message) = error {
            let text = scope.string(&message)?;
            scope.set(result, "error", text)?;
        }
        Ok(scope.finish(result))
    })
}

/// One stream step: consumed input, produced output, or a stream error.
fn run_stream(
    handle: &mut Handle,
    flush: i32,
    input: &[u8],
    output: &mut [u8],
) -> Result<(usize, usize), String> {
    if !handle.initialized {
        return Err("zlib handle is not initialized".to_string());
    }
    // Bytes the stream produced between calls go out first, in order.
    let mut carried = 0;
    if !handle.carry.is_empty() {
        carried = handle.carry.len().min(output.len());
        output[..carried].copy_from_slice(&handle.carry[..carried]);
        handle.carry.drain(..carried);
        if carried == output.len() {
            return Ok((0, carried));
        }
    }
    let output = &mut output[carried..];
    if handle.finished {
        return Ok((0, carried));
    }

    if is_codec(handle.mode) {
        let produced = run_codec(handle, flush, input, output)?;
        return Ok((input.len(), produced + carried));
    }
    let deflating = is_deflate(handle.mode);
    let stream = &raw mut *handle.stream.0;
    // SAFETY: both buffers outlive the call, and the stream is live and
    // initialized for the mode being driven.
    let (status, consumed, produced) = unsafe {
        (*stream).next_in = input.as_ptr();
        (*stream).avail_in = input.len() as u32;
        (*stream).next_out = output.as_mut_ptr();
        (*stream).avail_out = output.len() as u32;
        let status = if deflating {
            z::deflate(stream, flush)
        } else {
            z::inflate(stream, flush)
        };
        let consumed = input.len() - (*stream).avail_in as usize;
        let produced = output.len() - (*stream).avail_out as usize;
        (*stream).next_in = std::ptr::null_mut();
        (*stream).avail_in = 0;
        (*stream).next_out = std::ptr::null_mut();
        (*stream).avail_out = 0;
        (status, consumed, produced)
    };

    match status {
        z::Z_OK | z::Z_BUF_ERROR => Ok((consumed, produced + carried)),
        z::Z_STREAM_END => {
            handle.finished = true;
            Ok((consumed, produced + carried))
        }
        z::Z_NEED_DICT => {
            if handle.dictionary.is_empty() {
                return Err("Missing dictionary".to_string());
            }
            // SAFETY: the dictionary outlives the call; the stream is live.
            let status = unsafe {
                z::inflateSetDictionary(
                    stream,
                    handle.dictionary.as_ptr(),
                    handle.dictionary.len() as u32,
                )
            };
            if status != z::Z_OK {
                return Err("Bad dictionary".to_string());
            }
            // zlib asked for the dictionary mid-stream; with it in place
            // the rest of this call's input inflates now, so the caller
            // never sees a step that made no progress.
            let (status, more_in, more_out) = unsafe {
                (*stream).next_in = input[consumed..].as_ptr();
                (*stream).avail_in = (input.len() - consumed) as u32;
                (*stream).next_out = output[produced..].as_mut_ptr();
                (*stream).avail_out = (output.len() - produced) as u32;
                let status = z::inflate(stream, flush);
                let more_in = (input.len() - consumed) - (*stream).avail_in as usize;
                let more_out = (output.len() - produced) - (*stream).avail_out as usize;
                (*stream).next_in = std::ptr::null_mut();
                (*stream).avail_in = 0;
                (*stream).next_out = std::ptr::null_mut();
                (*stream).avail_out = 0;
                (status, more_in, more_out)
            };
            match status {
                z::Z_OK | z::Z_BUF_ERROR => {}
                z::Z_STREAM_END => handle.finished = true,
                other => return Err(format!("zlib error {other}")),
            }
            Ok((consumed + more_in, produced + more_out + carried))
        }
        z::Z_DATA_ERROR => Err(stream_message(handle).unwrap_or("incorrect data check".into())),
        z::Z_MEM_ERROR => Err("insufficient memory".to_string()),
        z::Z_STREAM_ERROR => Err("stream error".to_string()),
        other => Err(format!("zlib error {other}")),
    }
}

/// zlib's own message for the stream's last error, when it left one.
fn stream_message(handle: &Handle) -> Option<String> {
    let message = handle.stream.0.msg;
    if message.is_null() {
        return None;
    }
    // SAFETY: zlib leaves a NUL-terminated static string in `msg`.
    let text = unsafe { std::ffi::CStr::from_ptr(message) };
    Some(text.to_string_lossy().into_owned())
}

/// One codec step. The codec writes into its own buffer, and as much of
/// that as fits goes into the caller's output; the rest is carried, the
/// same way a zlib parameter change is.
fn run_codec(
    handle: &mut Handle,
    flush: i32,
    input: &[u8],
    output: &mut [u8],
) -> Result<usize, String> {
    use std::io::Write;

    let mut drained = Vec::new();
    match handle.codec.as_mut() {
        Some(Codec::BrotliEncode(writer)) => {
            writer.write_all(input).map_err(|error| error.to_string())?;
            match flush {
                BROTLI_OPERATION_FLUSH | BROTLI_OPERATION_FINISH => {
                    writer.flush().map_err(|error| error.to_string())?;
                }
                BROTLI_OPERATION_PROCESS => {}
                _ => {}
            }
            drained.append(writer.get_mut());
            if flush == BROTLI_OPERATION_FINISH {
                handle.finished = true;
            }
        }
        Some(Codec::BrotliDecode(writer)) => {
            writer
                .write_all(input)
                .map_err(|_| "Brotli decompression failed".to_string())?;
            writer
                .flush()
                .map_err(|_| "Brotli decompression failed".to_string())?;
            drained.append(writer.get_mut());
        }
        Some(Codec::ZstdCompress(context)) => {
            let mut buffer = Vec::with_capacity(input.len() + 128);
            let mut in_buffer = zstd_safe::InBuffer::around(input);
            loop {
                let mut out_buffer = zstd_safe::OutBuffer::around(&mut buffer);
                let directive = match flush {
                    ZSTD_E_FLUSH => zstd_safe::zstd_sys::ZSTD_EndDirective::ZSTD_e_flush,
                    ZSTD_E_END => zstd_safe::zstd_sys::ZSTD_EndDirective::ZSTD_e_end,
                    _ => zstd_safe::zstd_sys::ZSTD_EndDirective::ZSTD_e_continue,
                };
                let remaining = context
                    .compress_stream2(&mut out_buffer, &mut in_buffer, directive)
                    .map_err(|code| format!("zstd error {code}"))?;
                let produced = out_buffer.as_slice().len();
                drained.extend_from_slice(&buffer[..produced]);
                buffer.clear();
                if remaining == 0 && in_buffer.pos() == input.len() {
                    break;
                }
                if produced == 0 && in_buffer.pos() == input.len() && flush == ZSTD_E_CONTINUE {
                    break;
                }
            }
            if flush == ZSTD_E_END {
                handle.finished = true;
            }
        }
        Some(Codec::ZstdDecompress(context)) => {
            let mut in_buffer = zstd_safe::InBuffer::around(input);
            let mut buffer = Vec::with_capacity(input.len() * 4 + 128);
            loop {
                let mut out_buffer = zstd_safe::OutBuffer::around(&mut buffer);
                let hint = context
                    .decompress_stream(&mut out_buffer, &mut in_buffer)
                    .map_err(|code| format!("zstd error {code}"))?;
                let produced = out_buffer.as_slice().len();
                drained.extend_from_slice(&buffer[..produced]);
                buffer.clear();
                if in_buffer.pos() == input.len() && (produced == 0 || hint == 0) {
                    break;
                }
            }
        }
        None => return Err("codec handle is not initialized".to_string()),
    }

    let direct = drained.len().min(output.len());
    output[..direct].copy_from_slice(&drained[..direct]);
    if direct < drained.len() {
        handle.carry.extend_from_slice(&drained[direct..]);
    }
    Ok(direct)
}
