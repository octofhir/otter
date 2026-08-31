//! Caller-buffered Brotli and Zstandard streaming steps for `node:zlib`.
//!
//! # Contents
//! - [`Codec`] owns the state of one non-zlib compressor or decompressor.
//! - [`Codec::process`] advances that state directly into the caller's output.
//!
//! # Invariants
//! - A step never accumulates decoded or encoded output in a handle-owned `Vec`.
//! - Input consumption and output production describe exactly one codec step,
//!   so the JavaScript `Transform` loop remains the backpressure boundary.
//! - Brotli windows and Zstandard decoder windows have finite internal maxima.
//!
//! # See also
//! - [`super`] adapts these steps to Node's native zlib handle contract.

use brotli::enc::StandardAlloc;
use brotli::enc::encode::{
    BrotliEncoderOperation, BrotliEncoderParameter, BrotliEncoderStateStruct,
};
use brotli::{BrotliDecompressStream, BrotliResult, BrotliState};

const BROTLI_OPERATION_FLUSH: i32 = 1;
const BROTLI_OPERATION_FINISH: i32 = 2;
const ZSTD_E_CONTINUE: i32 = 0;
const ZSTD_E_FLUSH: i32 = 1;
const ZSTD_E_END: i32 = 2;
/// Refuse frames that require a decoder window larger than 16 MiB.
const MAX_ZSTD_WINDOW_LOG: u32 = 24;

type BrotliDecoderState = BrotliState<StandardAlloc, StandardAlloc, StandardAlloc>;

pub(super) struct BrotliEncoder {
    state: BrotliEncoderStateStruct<StandardAlloc>,
    total_out: Option<usize>,
}

pub(super) struct BrotliDecoder {
    state: BrotliDecoderState,
    total_out: usize,
}

/// A live non-zlib codec state.
pub(super) enum Codec {
    BrotliEncode(Box<BrotliEncoder>),
    BrotliDecode(Box<BrotliDecoder>),
    ZstdCompress(Box<zstd_safe::CCtx<'static>>),
    ZstdDecompress(Box<zstd_safe::DCtx<'static>>),
}

// SAFETY: a codec is reachable only through the handle table's mutex and is
// driven from the isolate thread that created the table.
unsafe impl Send for Codec {}

/// Exact progress made by one caller-buffered codec step.
pub(super) struct CodecStep {
    pub(super) consumed: usize,
    pub(super) produced: usize,
    pub(super) finished: bool,
}

impl Codec {
    pub(super) fn brotli_encode(quality: u32, window_bits: u32) -> Self {
        let mut state = BrotliEncoderStateStruct::new(StandardAlloc::default());
        state.set_parameter(
            BrotliEncoderParameter::BROTLI_PARAM_QUALITY,
            quality.min(11),
        );
        state.set_parameter(
            BrotliEncoderParameter::BROTLI_PARAM_LGWIN,
            window_bits.clamp(10, 24),
        );
        Self::BrotliEncode(Box::new(BrotliEncoder {
            state,
            total_out: Some(0),
        }))
    }

    pub(super) fn brotli_decode() -> Self {
        Self::BrotliDecode(Box::new(BrotliDecoder {
            state: BrotliState::new_strict(
                StandardAlloc::default(),
                StandardAlloc::default(),
                StandardAlloc::default(),
            ),
            total_out: 0,
        }))
    }

    pub(super) fn zstd_compress(level: i32) -> Result<Self, String> {
        let mut context = zstd_safe::CCtx::create();
        if level > 0 {
            context
                .set_parameter(zstd_safe::CParameter::CompressionLevel(level))
                .map_err(|code| format!("zstd error {code}"))?;
        }
        Ok(Self::ZstdCompress(Box::new(context)))
    }

    pub(super) fn zstd_decompress() -> Result<Self, String> {
        let mut context = zstd_safe::DCtx::create();
        context
            .set_parameter(zstd_safe::DParameter::WindowLogMax(MAX_ZSTD_WINDOW_LOG))
            .map_err(|code| format!("zstd error {code}"))?;
        Ok(Self::ZstdDecompress(Box::new(context)))
    }

    /// Advance the codec directly into `output` without retaining produced bytes.
    pub(super) fn process(
        &mut self,
        flush: i32,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<CodecStep, String> {
        match self {
            Self::BrotliEncode(encoder) => process_brotli_encode(encoder, flush, input, output),
            Self::BrotliDecode(decoder) => process_brotli_decode(decoder, flush, input, output),
            Self::ZstdCompress(context) => process_zstd_compress(context, flush, input, output),
            Self::ZstdDecompress(context) => process_zstd_decompress(context, input, output),
        }
    }
}

fn process_brotli_encode(
    encoder: &mut BrotliEncoder,
    flush: i32,
    input: &[u8],
    output: &mut [u8],
) -> Result<CodecStep, String> {
    let operation = match flush {
        BROTLI_OPERATION_FLUSH => BrotliEncoderOperation::BROTLI_OPERATION_FLUSH,
        BROTLI_OPERATION_FINISH => BrotliEncoderOperation::BROTLI_OPERATION_FINISH,
        _ => BrotliEncoderOperation::BROTLI_OPERATION_PROCESS,
    };
    let mut available_in = input.len();
    let mut input_offset = 0;
    let mut available_out = output.len();
    let mut output_offset = 0;
    let mut no_metadata =
        |_: &mut brotli::interface::PredictionModeContextMap<brotli::InputReferenceMut>,
         _: &mut [brotli::interface::StaticCommand],
         _: brotli::InputPair,
         _: &mut StandardAlloc| {};
    let ok = encoder.state.compress_stream(
        operation,
        &mut available_in,
        input,
        &mut input_offset,
        &mut available_out,
        output,
        &mut output_offset,
        &mut encoder.total_out,
        &mut no_metadata,
    );
    if !ok {
        return Err("Brotli compression failed".to_string());
    }
    Ok(CodecStep {
        consumed: input_offset,
        produced: output_offset,
        finished: encoder.state.is_finished(),
    })
}

fn process_brotli_decode(
    decoder: &mut BrotliDecoder,
    flush: i32,
    input: &[u8],
    output: &mut [u8],
) -> Result<CodecStep, String> {
    let mut available_in = input.len();
    let mut input_offset = 0;
    let mut available_out = output.len();
    let mut output_offset = 0;
    let result = BrotliDecompressStream(
        &mut available_in,
        &mut input_offset,
        input,
        &mut available_out,
        &mut output_offset,
        output,
        &mut decoder.total_out,
        &mut decoder.state,
    );
    match result {
        BrotliResult::ResultFailure => Err("Brotli decompression failed".to_string()),
        BrotliResult::NeedsMoreInput
            if flush == BROTLI_OPERATION_FINISH && input_offset == input.len() =>
        {
            Err("unexpected end of Brotli stream".to_string())
        }
        BrotliResult::ResultSuccess => Ok(CodecStep {
            consumed: input_offset,
            produced: output_offset,
            finished: true,
        }),
        BrotliResult::NeedsMoreInput | BrotliResult::NeedsMoreOutput => Ok(CodecStep {
            consumed: input_offset,
            produced: output_offset,
            finished: false,
        }),
    }
}

fn process_zstd_compress(
    context: &mut zstd_safe::CCtx<'static>,
    flush: i32,
    input: &[u8],
    output: &mut [u8],
) -> Result<CodecStep, String> {
    let directive = match flush {
        ZSTD_E_FLUSH => zstd_safe::zstd_sys::ZSTD_EndDirective::ZSTD_e_flush,
        ZSTD_E_END => zstd_safe::zstd_sys::ZSTD_EndDirective::ZSTD_e_end,
        _ => zstd_safe::zstd_sys::ZSTD_EndDirective::ZSTD_e_continue,
    };
    let mut input_buffer = zstd_safe::InBuffer::around(input);
    let mut produced = 0;
    let mut finished = false;

    loop {
        let before_input = input_buffer.pos();
        let mut output_buffer = zstd_safe::OutBuffer::around(&mut output[produced..]);
        let remaining = context
            .compress_stream2(&mut output_buffer, &mut input_buffer, directive)
            .map_err(|code| format!("zstd error {code}"))?;
        let step_output = output_buffer.pos();
        produced += step_output;

        if flush == ZSTD_E_END && remaining == 0 && input_buffer.pos() == input.len() {
            finished = true;
            break;
        }
        if produced == output.len() {
            break;
        }
        if input_buffer.pos() == input.len() && (flush == ZSTD_E_CONTINUE || remaining == 0) {
            break;
        }
        if input_buffer.pos() == before_input && step_output == 0 {
            return Err("zstd stream made no progress".to_string());
        }
    }

    Ok(CodecStep {
        consumed: input_buffer.pos(),
        produced,
        finished,
    })
}

fn process_zstd_decompress(
    context: &mut zstd_safe::DCtx<'static>,
    input: &[u8],
    output: &mut [u8],
) -> Result<CodecStep, String> {
    let mut input_buffer = zstd_safe::InBuffer::around(input);
    let mut produced = 0;
    let mut finished = false;

    loop {
        let before_input = input_buffer.pos();
        let mut output_buffer = zstd_safe::OutBuffer::around(&mut output[produced..]);
        let hint = context
            .decompress_stream(&mut output_buffer, &mut input_buffer)
            .map_err(|code| format!("zstd error {code}"))?;
        let step_output = output_buffer.pos();
        produced += step_output;

        if hint == 0 {
            finished = true;
            break;
        }
        if produced == output.len() || input_buffer.pos() == input.len() {
            break;
        }
        if input_buffer.pos() == before_input && step_output == 0 {
            return Err("zstd stream made no progress".to_string());
        }
    }

    Ok(CodecStep {
        consumed: input_buffer.pos(),
        produced,
        finished,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finish_codec(codec: &mut Codec, input: &[u8], output_chunk: usize) -> Vec<u8> {
        let mut consumed = 0;
        let mut output = Vec::new();
        for _ in 0..100_000 {
            let mut chunk = vec![0; output_chunk];
            let step = codec
                .process(BROTLI_OPERATION_FINISH, &input[consumed..], &mut chunk)
                .expect("codec step");
            assert!(step.produced <= output_chunk);
            consumed += step.consumed;
            output.extend_from_slice(&chunk[..step.produced]);
            if step.finished {
                assert_eq!(consumed, input.len());
                return output;
            }
            assert!(step.consumed != 0 || step.produced != 0, "codec stalled");
        }
        panic!("codec did not finish");
    }

    #[test]
    fn brotli_round_trip_uses_only_caller_output_chunks() {
        let source = b"otter-streaming-brotli-".repeat(4096);
        let mut encoder = Codec::brotli_encode(6, 20);
        let compressed = finish_codec(&mut encoder, &source, 37);
        let mut decoder = Codec::brotli_decode();
        let decoded = finish_codec(&mut decoder, &compressed, 31);
        assert_eq!(decoded, source);
    }

    #[test]
    fn zstd_round_trip_uses_only_caller_output_chunks() {
        let source = b"otter-streaming-zstd-".repeat(4096);
        let mut encoder = Codec::zstd_compress(3).expect("zstd encoder");
        let compressed = finish_codec(&mut encoder, &source, 41);
        let mut decoder = Codec::zstd_decompress().expect("zstd decoder");
        let decoded = finish_codec(&mut decoder, &compressed, 29);
        assert_eq!(decoded, source);
    }
}
