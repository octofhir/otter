//! Flat binary encoding of a compiled module.
//!
//! A cache only pays if reading an entry costs much less than producing it,
//! and a general-purpose serializer does not clear that bar here: it walks the
//! module as a graph of values, allocating as it goes, which lands in the same
//! order of magnitude as simply compiling the source again. The engines that
//! ship code caches do not do that — V8 writes a flat blob it can fix up in a
//! near-linear pass, and JSC's bytecode cache is a flat buffer of records read
//! back by offset.
//!
//! This is that shape. Every value is written little-endian at a known width,
//! every sequence is length-prefixed, and decoding is a straight walk over the
//! bytes into pre-sized vectors — no visitor indirection, no reallocation, no
//! per-node dispatch.
//!
//! # Contents
//! - [`encode_module`] — a module as flat bytes.
//! - [`encode_module_bounded`] — the same encoding with a hard output budget.
//! - [`decode_module`] — flat bytes into an immutable verified carrier.
//!
//! # Invariants
//! - Encoding validates every wire-length conversion. The bounded entry point
//!   stops before an append would exceed its caller's byte budget; it never
//!   constructs an oversized buffer and rejects it afterwards.
//! - Any byte string that is not something [`encode_module`] produced fails
//!   to decode rather than producing a wrong module.
//! - Reads are bounds-checked. A truncated or corrupt buffer returns a typed
//!   error and never creates a [`VerifiedBytecodeModule`] or reaches a VM
//!   consumer.
//! - The format carries no version. A reader that cannot make sense of a
//!   buffer rejects it, and the caller — which keys entries by the build that
//!   wrote them — simply produces the module again.
//! - Every decoded `String` and `Vec` shares one allocation budget. The budget
//!   has both an input-linear quota and a fixed hard ceiling, so a corrupt
//!   count cannot amplify a small cache blob into a large allocation.
//!
//! # See also
//! - [`crate::BytecodeModule`], the value this encodes.

use crate::wordcode::{FunctionCode, INLINE_OPERAND_WORDS, Instruction};
use crate::{
    ArgumentBindingStorage, ArgumentsObjectKind, BytecodeModule, ClassHintSite, Constant,
    DirectEvalBinding, Function, MappedArgumentBinding, ModuleInit, ModuleResolution, SourceKind,
    SpanEntry, TemplateSite,
    encoding::{op_from_byte, op_to_byte},
    verifier::{BytecodeVerifyError, VerifiedBytecodeModule},
};

/// Marks a buffer as this encoding. A buffer that does not start with it is
/// not something this module wrote.
const MAGIC: &[u8; 8] = b"otterbc\0";

/// Small modules need enough headroom for owned Rust collection headers, while
/// larger modules should remain proportional to their flat representation.
const DECODE_ALLOCATION_FLOOR_BYTES: usize = 64 * 1024;
const DECODE_ALLOCATION_BYTES_PER_INPUT_BYTE: usize = 16;
const DECODE_ALLOCATION_HARD_LIMIT_BYTES: usize = 256 * 1024 * 1024;

/// Encode `module` as flat bytes.
///
/// Callers accepting modules from a bounded or otherwise untrusted source
/// should use [`encode_module_bounded`]. This convenience entry point preserves
/// the historical infallible API for already-admitted in-memory modules.
#[must_use]
pub fn encode_module(module: &BytecodeModule) -> Vec<u8> {
    encode_module_with_limit(module, usize::MAX)
        .unwrap_or_else(|error| panic!("failed to encode admitted bytecode module: {error}"))
}

/// Encode `module` without ever growing the output beyond `max_bytes`.
///
/// # Errors
/// Returns [`ModuleEncodeError`] when the wire representation cannot fit its
/// fixed-width length fields, would exceed `max_bytes`, or allocation fails.
pub fn encode_module_bounded(
    module: &BytecodeModule,
    max_bytes: usize,
) -> Result<Vec<u8>, ModuleEncodeError> {
    encode_module_with_limit(module, max_bytes)
}

fn encode_module_with_limit(
    module: &BytecodeModule,
    max_bytes: usize,
) -> Result<Vec<u8>, ModuleEncodeError> {
    let mut out = Writer::new(max_bytes);
    out.bytes(MAGIC);
    out.string(&module.module);
    out.u8(source_kind_tag(module.source_kind));
    out.seq(&module.template_sites, Writer::template_site);
    out.seq(&module.functions, Writer::function);
    out.seq(&module.constants, Writer::constant);
    out.seq(&module.module_resolutions, Writer::module_resolution);
    out.seq(&module.module_inits, Writer::module_init);
    out.optional_string(module.function_source.as_ref());
    out.finish()
}

/// Typed reason a module could not be represented as bounded flat bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModuleEncodeError {
    /// A string or sequence length does not fit the format's `u32` field.
    LengthOverflow,
    /// The next complete field would exceed the caller's output budget.
    SizeLimitExceeded,
    /// Reserving the required output storage failed.
    AllocationFailed,
}

impl std::fmt::Display for ModuleEncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LengthOverflow => write!(f, "bytecode field length exceeds u32"),
            Self::SizeLimitExceeded => write!(f, "encoded bytecode exceeds output limit"),
            Self::AllocationFailed => write!(f, "encoded bytecode allocation failed"),
        }
    }
}

impl std::error::Error for ModuleEncodeError {}

/// Typed reason a flat bytecode blob was rejected.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModuleDecodeError {
    /// The buffer is truncated, malformed, or has trailing data.
    MalformedEncoding,
    /// The flat shape decoded, but the module violates VM admission
    /// invariants.
    Verify(BytecodeVerifyError),
}

impl std::fmt::Display for ModuleDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MalformedEncoding => write!(f, "malformed flat bytecode encoding"),
            Self::Verify(error) => write!(f, "invalid bytecode module: {error}"),
        }
    }
}

impl std::error::Error for ModuleDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::MalformedEncoding => None,
            Self::Verify(error) => Some(error),
        }
    }
}

/// Decode and verify a normal base-zero module.
///
/// # Errors
/// Returns [`ModuleDecodeError`] for malformed bytes or a structurally invalid
/// module.
pub fn decode_module(bytes: &[u8]) -> Result<VerifiedBytecodeModule, ModuleDecodeError> {
    let module = decode_unverified_module(bytes).ok_or(ModuleDecodeError::MalformedEncoding)?;
    VerifiedBytecodeModule::new(module).map_err(ModuleDecodeError::Verify)
}

fn decode_unverified_module(bytes: &[u8]) -> Option<BytecodeModule> {
    let mut input = Reader::new(bytes);
    if input.bytes(MAGIC.len())? != MAGIC {
        return None;
    }
    let module = input.string()?;
    let source_kind = source_kind_from_tag(input.u8()?)?;
    let template_sites = input.seq(Reader::template_site)?;
    let functions = input.seq(Reader::function)?;
    let constants = input.seq(Reader::constant)?;
    let module_resolutions = input.seq(Reader::module_resolution)?;
    let module_inits = input.seq(Reader::module_init)?;
    let function_source = input.optional_string()?;
    if !input.is_at_end() {
        return None;
    }
    Some(BytecodeModule {
        module,
        template_sites,
        source_kind,
        functions,
        function_source,
        constants,
        module_resolutions,
        module_inits,
    })
}

struct Writer {
    out: Vec<u8>,
    max_bytes: usize,
    error: Option<ModuleEncodeError>,
}

impl Writer {
    fn new(max_bytes: usize) -> Self {
        let initial_capacity = max_bytes.min(64 * 1024);
        let mut out = Vec::new();
        let error = out
            .try_reserve_exact(initial_capacity)
            .err()
            .map(|_| ModuleEncodeError::AllocationFailed);
        Self {
            out,
            max_bytes,
            error,
        }
    }

    fn finish(self) -> Result<Vec<u8>, ModuleEncodeError> {
        self.error.map_or(Ok(self.out), Err)
    }

    fn has_failed(&self) -> bool {
        self.error.is_some()
    }

    fn set_error(&mut self, error: ModuleEncodeError) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        if self.has_failed() {
            return;
        }
        let Some(new_len) = self.out.len().checked_add(bytes.len()) else {
            self.set_error(ModuleEncodeError::SizeLimitExceeded);
            return;
        };
        if new_len > self.max_bytes {
            self.set_error(ModuleEncodeError::SizeLimitExceeded);
            return;
        }
        let spare = self.out.capacity().saturating_sub(self.out.len());
        if spare < bytes.len() {
            let target_capacity = self
                .out
                .capacity()
                .saturating_mul(2)
                .max(new_len)
                .min(self.max_bytes);
            let additional = target_capacity.saturating_sub(self.out.len());
            if self.out.try_reserve_exact(additional).is_err() {
                self.set_error(ModuleEncodeError::AllocationFailed);
                return;
            }
        }
        self.out.extend_from_slice(bytes);
    }

    fn wire_len(&mut self, len: usize) -> Option<u32> {
        match u32::try_from(len) {
            Ok(len) => Some(len),
            Err(_) => {
                self.set_error(ModuleEncodeError::LengthOverflow);
                None
            }
        }
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.append(bytes);
    }

    fn u8(&mut self, value: u8) {
        self.append(&[value]);
    }

    fn bool(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    fn u16(&mut self, value: u16) {
        self.append(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.append(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.append(&value.to_le_bytes());
    }

    fn span(&mut self, span: (u32, u32)) {
        self.u32(span.0);
        self.u32(span.1);
    }

    fn string(&mut self, value: &str) {
        let Some(len) = self.wire_len(value.len()) else {
            return;
        };
        self.u32(len);
        self.bytes(value.as_bytes());
    }

    fn optional_string(&mut self, value: Option<&String>) {
        match value {
            Some(value) => {
                self.bool(true);
                self.string(value);
            }
            None => self.bool(false),
        }
    }

    fn utf16(&mut self, units: &[u16]) {
        let Some(len) = self.wire_len(units.len()) else {
            return;
        };
        self.u32(len);
        for unit in units {
            if self.has_failed() {
                break;
            }
            self.u16(*unit);
        }
    }

    fn seq<T>(&mut self, items: &[T], mut write: impl FnMut(&mut Self, &T)) {
        let Some(len) = self.wire_len(items.len()) else {
            return;
        };
        self.u32(len);
        for item in items {
            if self.has_failed() {
                break;
            }
            write(self, item);
        }
    }

    fn u32_seq(&mut self, items: &[u32]) {
        let Some(len) = self.wire_len(items.len()) else {
            return;
        };
        self.u32(len);
        for item in items {
            if self.has_failed() {
                break;
            }
            self.u32(*item);
        }
    }

    fn template_site(&mut self, site: &TemplateSite) {
        let Some(len) = self.wire_len(site.cooked.len()) else {
            return;
        };
        self.u32(len);
        for cooked in &site.cooked {
            if self.has_failed() {
                break;
            }
            self.optional_string(cooked.as_ref());
        }
        self.seq(&site.raw, |writer, raw| writer.string(raw));
    }

    fn constant(&mut self, constant: &Constant) {
        match constant {
            Constant::String { utf16 } => {
                self.u8(0);
                self.utf16(utf16);
            }
            Constant::Number { bits } => {
                self.u8(1);
                self.u64(*bits);
            }
            Constant::FunctionId { index } => {
                self.u8(2);
                self.u32(*index);
            }
            Constant::BigInt { decimal } => {
                self.u8(3);
                self.string(decimal);
            }
            Constant::RegExp {
                pattern_utf16,
                flags,
            } => {
                self.u8(4);
                self.utf16(pattern_utf16);
                self.string(flags);
            }
        }
    }

    fn module_resolution(&mut self, edge: &ModuleResolution) {
        self.string(&edge.referrer);
        self.string(&edge.specifier);
        self.optional_string(edge.attr_type.as_ref());
        self.string(&edge.target);
        self.bool(edge.deferred);
        self.bool(edge.dynamic);
        self.bool(edge.synthetic);
    }

    fn module_init(&mut self, init: &ModuleInit) {
        self.string(&init.url);
        self.u32(init.function_id);
    }

    fn span_entry(&mut self, entry: &SpanEntry) {
        self.u32(entry.pc);
        self.span(entry.span);
    }

    fn class_hint_site(&mut self, site: &ClassHintSite) {
        self.u32(site.pc);
        self.u32(site.class_function_id);
    }

    fn direct_eval_binding(&mut self, binding: &DirectEvalBinding) {
        self.bool(binding.captured);
        self.string(&binding.name);
        self.u16(binding.upvalue);
        self.bool(binding.lexical);
        self.bool(binding.is_const);
        self.bool(binding.fn_self_name);
        self.bool(binding.inner);
        self.bool(binding.param);
        self.bool(binding.deletable);
        self.u16(binding.scope_depth);
    }

    fn mapped_argument_binding(&mut self, binding: &MappedArgumentBinding) {
        self.u16(binding.argument_index);
        self.string(&binding.formal_name);
        match binding.storage {
            ArgumentBindingStorage::Register { reg } => {
                self.u8(0);
                self.u16(reg);
            }
            ArgumentBindingStorage::Upvalue { idx } => {
                self.u8(1);
                self.u16(idx);
            }
        }
    }

    fn function_code(&mut self, code: &FunctionCode) {
        let (instructions, overflow) = code.raw_parts();
        let Some(len) = self.wire_len(instructions.len()) else {
            return;
        };
        self.u32(len);
        for instruction in instructions {
            if self.has_failed() {
                break;
            }
            let (op, operand_count, inline, overflow_offset) = instruction.raw_parts();
            // The wire byte, not the Rust discriminant: the two orders differ,
            // and reading one as the other lands on a different opcode whose
            // operands then decode against the wrong schema row.
            self.u8(op_to_byte(op).unwrap_or(u8::MAX));
            self.u8(operand_count);
            for word in inline {
                self.u32(word);
            }
            self.u32(overflow_offset);
        }
        self.u32_seq(overflow);
    }

    fn function(&mut self, function: &Function) {
        self.u32(function.id);
        self.string(&function.name);
        self.span(function.span);
        self.u16(function.locals);
        self.u16(function.scratch);
        self.u16(function.param_count);
        self.u16(function.length);
        self.u16(function.own_upvalue_count);
        self.u16(function.inherited_upvalue_count);
        for flag in [
            function.is_strict,
            function.is_arrow,
            function.is_method,
            function.has_rest,
            function.is_async,
            function.is_generator,
            function.is_async_generator,
            function.is_module,
            function.is_derived_constructor,
            function.needs_arguments,
            function.uses_arguments_callee,
            function.contains_direct_eval,
        ] {
            self.bool(flag);
        }
        self.u8(match function.arguments_object_kind {
            ArgumentsObjectKind::Unmapped => 0,
            ArgumentsObjectKind::Mapped => 1,
        });
        self.seq(
            &function.mapped_argument_bindings,
            Self::mapped_argument_binding,
        );
        self.string(&function.module_url);
        self.seq(&function.direct_eval_bindings, Self::direct_eval_binding);
        self.u32(function.eval_sites.len() as u32);
        for site in &function.eval_sites {
            self.seq(site, Self::direct_eval_binding);
        }
        match function.source_text_range {
            Some(range) => {
                self.bool(true);
                self.span(range);
            }
            None => self.bool(false),
        }
        match function.source_text_span {
            Some(span) => {
                self.bool(true);
                self.span(span);
            }
            None => self.bool(false),
        }
        self.function_code(&function.code);
        self.seq(&function.spans, Self::span_entry);
        self.u32_seq(&function.number_hint_sites);
        self.seq(&function.class_hint_sites, Self::class_hint_site);
    }
}

struct Reader<'a> {
    input: &'a [u8],
    at: usize,
    allocation_budget: usize,
}

impl<'a> Reader<'a> {
    fn new(input: &'a [u8]) -> Self {
        let allocation_budget = input
            .len()
            .checked_mul(DECODE_ALLOCATION_BYTES_PER_INPUT_BYTE)
            .and_then(|bytes| bytes.checked_add(DECODE_ALLOCATION_FLOOR_BYTES))
            .unwrap_or(DECODE_ALLOCATION_HARD_LIMIT_BYTES)
            .min(DECODE_ALLOCATION_HARD_LIMIT_BYTES);
        Self {
            input,
            at: 0,
            allocation_budget,
        }
    }

    fn is_at_end(&self) -> bool {
        self.at == self.input.len()
    }

    fn remaining(&self) -> usize {
        self.input.len() - self.at
    }

    fn bytes(&mut self, count: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(count)?;
        let slice = self.input.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.bytes(1)?[0])
    }

    fn bool(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.bytes(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.bytes(8)?.try_into().ok()?))
    }

    fn span(&mut self) -> Option<(u32, u32)> {
        Some((self.u32()?, self.u32()?))
    }

    /// A sequence length, rejected when it exceeds what is left to read.
    ///
    /// `per_item` is the smallest number of bytes one item can occupy, so a
    /// corrupt count cannot make the reader pre-size a huge vector.
    fn count(&mut self, per_item: usize) -> Option<usize> {
        let count = self.u32()? as usize;
        if count.checked_mul(per_item.max(1))? > self.remaining() {
            return None;
        }
        Some(count)
    }

    fn charge_allocation(&mut self, bytes: usize) -> Option<()> {
        self.allocation_budget = self.allocation_budget.checked_sub(bytes)?;
        Some(())
    }

    fn vec_with_exact_capacity<T>(&mut self, count: usize) -> Option<Vec<T>> {
        let bytes = count.checked_mul(std::mem::size_of::<T>())?;
        self.charge_allocation(bytes)?;
        let mut items = Vec::new();
        items.try_reserve_exact(count).ok()?;
        Some(items)
    }

    fn string(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        let value = std::str::from_utf8(self.bytes(len)?).ok()?;
        self.charge_allocation(len)?;
        let mut decoded = String::new();
        decoded.try_reserve_exact(len).ok()?;
        decoded.push_str(value);
        Some(decoded)
    }

    fn optional_string(&mut self) -> Option<Option<String>> {
        if self.bool()? {
            Some(Some(self.string()?))
        } else {
            Some(None)
        }
    }

    fn utf16(&mut self) -> Option<Vec<u16>> {
        let count = self.count(2)?;
        let mut units = self.vec_with_exact_capacity(count)?;
        for _ in 0..count {
            units.push(self.u16()?);
        }
        Some(units)
    }

    fn seq<T>(&mut self, mut read: impl FnMut(&mut Self) -> Option<T>) -> Option<Vec<T>> {
        let count = self.count(1)?;
        let mut items = self.vec_with_exact_capacity(count)?;
        for _ in 0..count {
            items.push(read(self)?);
        }
        Some(items)
    }

    fn u32_seq(&mut self) -> Option<Vec<u32>> {
        let count = self.count(4)?;
        let mut items = self.vec_with_exact_capacity(count)?;
        for _ in 0..count {
            items.push(self.u32()?);
        }
        Some(items)
    }

    fn template_site(&mut self) -> Option<TemplateSite> {
        let cooked_count = self.count(1)?;
        let mut cooked = self.vec_with_exact_capacity(cooked_count)?;
        for _ in 0..cooked_count {
            cooked.push(self.optional_string()?);
        }
        let raw = self.seq(Self::string)?;
        Some(TemplateSite { cooked, raw })
    }

    fn constant(&mut self) -> Option<Constant> {
        Some(match self.u8()? {
            0 => Constant::String {
                utf16: self.utf16()?,
            },
            1 => Constant::Number { bits: self.u64()? },
            2 => Constant::FunctionId { index: self.u32()? },
            3 => Constant::BigInt {
                decimal: self.string()?,
            },
            4 => Constant::RegExp {
                pattern_utf16: self.utf16()?,
                flags: self.string()?,
            },
            _ => return None,
        })
    }

    fn module_resolution(&mut self) -> Option<ModuleResolution> {
        Some(ModuleResolution {
            referrer: self.string()?,
            specifier: self.string()?,
            attr_type: self.optional_string()?,
            target: self.string()?,
            deferred: self.bool()?,
            dynamic: self.bool()?,
            synthetic: self.bool()?,
        })
    }

    fn module_init(&mut self) -> Option<ModuleInit> {
        Some(ModuleInit {
            url: self.string()?,
            function_id: self.u32()?,
        })
    }

    fn span_entry(&mut self) -> Option<SpanEntry> {
        Some(SpanEntry {
            pc: self.u32()?,
            span: self.span()?,
        })
    }

    fn class_hint_site(&mut self) -> Option<ClassHintSite> {
        Some(ClassHintSite {
            pc: self.u32()?,
            class_function_id: self.u32()?,
        })
    }

    fn direct_eval_binding(&mut self) -> Option<DirectEvalBinding> {
        Some(DirectEvalBinding {
            captured: self.bool()?,
            name: self.string()?,
            upvalue: self.u16()?,
            lexical: self.bool()?,
            is_const: self.bool()?,
            fn_self_name: self.bool()?,
            inner: self.bool()?,
            param: self.bool()?,
            deletable: self.bool()?,
            scope_depth: self.u16()?,
        })
    }

    fn mapped_argument_binding(&mut self) -> Option<MappedArgumentBinding> {
        let argument_index = self.u16()?;
        let formal_name = self.string()?;
        let storage = match self.u8()? {
            0 => ArgumentBindingStorage::Register { reg: self.u16()? },
            1 => ArgumentBindingStorage::Upvalue { idx: self.u16()? },
            _ => return None,
        };
        Some(MappedArgumentBinding {
            argument_index,
            formal_name,
            storage,
        })
    }

    fn function_code(&mut self) -> Option<FunctionCode> {
        // One instruction occupies opcode + operand count + four inline words
        // + one overflow offset.
        let count = self.count(2 + INLINE_OPERAND_WORDS * 4 + 4)?;
        let mut instructions = self.vec_with_exact_capacity(count)?;
        for _ in 0..count {
            let op = op_from_byte(self.u8()?)?;
            let operand_count = self.u8()?;
            let mut inline = [0u32; INLINE_OPERAND_WORDS];
            for word in &mut inline {
                *word = self.u32()?;
            }
            let overflow_offset = self.u32()?;
            instructions.push(Instruction::from_raw_parts(
                op,
                operand_count,
                inline,
                overflow_offset,
            ));
        }
        let overflow = self.u32_seq()?;
        Some(FunctionCode::from_raw_parts(instructions, overflow))
    }

    fn function(&mut self) -> Option<Function> {
        let id = self.u32()?;
        let name = self.string()?;
        let span = self.span()?;
        let locals = self.u16()?;
        let scratch = self.u16()?;
        let param_count = self.u16()?;
        let length = self.u16()?;
        let own_upvalue_count = self.u16()?;
        let inherited_upvalue_count = self.u16()?;
        let is_strict = self.bool()?;
        let is_arrow = self.bool()?;
        let is_method = self.bool()?;
        let has_rest = self.bool()?;
        let is_async = self.bool()?;
        let is_generator = self.bool()?;
        let is_async_generator = self.bool()?;
        let is_module = self.bool()?;
        let is_derived_constructor = self.bool()?;
        let needs_arguments = self.bool()?;
        let uses_arguments_callee = self.bool()?;
        let contains_direct_eval = self.bool()?;
        let arguments_object_kind = match self.u8()? {
            0 => ArgumentsObjectKind::Unmapped,
            1 => ArgumentsObjectKind::Mapped,
            _ => return None,
        };
        let mapped_argument_bindings = self.seq(Self::mapped_argument_binding)?;
        let module_url = self.string()?;
        let direct_eval_bindings = self.seq(Self::direct_eval_binding)?;
        let site_count = self.u32()? as usize;
        let mut eval_sites = Vec::with_capacity(site_count);
        for _ in 0..site_count {
            eval_sites.push(self.seq(Self::direct_eval_binding)?);
        }
        let source_text_range = if self.bool()? {
            Some(self.span()?)
        } else {
            None
        };
        let source_text_span = if self.bool()? {
            Some(self.span()?)
        } else {
            None
        };
        let code = self.function_code()?;
        let spans = self.seq(Self::span_entry)?;
        let number_hint_sites = self.u32_seq()?;
        let class_hint_sites = self.seq(Self::class_hint_site)?;
        Some(Function {
            id,
            name,
            span,
            locals,
            scratch,
            param_count,
            length,
            own_upvalue_count,
            inherited_upvalue_count,
            is_strict,
            is_arrow,
            is_method,
            has_rest,
            is_async,
            is_generator,
            is_async_generator,
            is_module,
            is_derived_constructor,
            needs_arguments,
            uses_arguments_callee,
            arguments_object_kind,
            mapped_argument_bindings,
            module_url,
            direct_eval_bindings,
            eval_sites,
            contains_direct_eval,
            source_text_range,
            source_text_span,
            code,
            spans,
            number_hint_sites,
            class_hint_sites,
        })
    }
}

const fn source_kind_tag(kind: SourceKind) -> u8 {
    match kind {
        SourceKind::JavaScript => 0,
        SourceKind::TypeScript => 1,
    }
}

const fn source_kind_from_tag(tag: u8) -> Option<SourceKind> {
    match tag {
        0 => Some(SourceKind::JavaScript),
        1 => Some(SourceKind::TypeScript),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FunctionCodeBuilder, Op, Operand};

    fn sample_module() -> BytecodeModule {
        let mut code = FunctionCodeBuilder::new();
        code.push(Op::LoadUndefined, &[Operand::Register(0)]);
        code.push(Op::Return, &[Operand::Register(0)]);
        BytecodeModule {
            module: "file:///entry.ts".to_string(),
            template_sites: vec![TemplateSite {
                cooked: vec![Some("a".to_string()), None],
                raw: vec!["a".to_string(), "\\u{}".to_string()],
            }],
            source_kind: SourceKind::TypeScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".to_string(),
                span: (0, 12),
                locals: 2,
                scratch: 1,
                param_count: 1,
                length: 1,
                own_upvalue_count: 1,
                inherited_upvalue_count: 2,
                is_strict: true,
                is_arrow: false,
                is_method: true,
                has_rest: false,
                is_async: true,
                is_generator: false,
                is_async_generator: false,
                is_module: true,
                is_derived_constructor: false,
                needs_arguments: true,
                uses_arguments_callee: false,
                arguments_object_kind: ArgumentsObjectKind::Mapped,
                mapped_argument_bindings: vec![MappedArgumentBinding {
                    argument_index: 0,
                    formal_name: "x".to_string(),
                    storage: ArgumentBindingStorage::Upvalue { idx: 0 },
                }],
                module_url: "file:///entry.ts".to_string(),
                direct_eval_bindings: vec![DirectEvalBinding {
                    captured: true,
                    name: "outer".to_string(),
                    upvalue: 1,
                    lexical: true,
                    is_const: false,
                    fn_self_name: false,
                    inner: false,
                    param: false,
                    deletable: false,
                    scope_depth: 1,
                }],
                eval_sites: vec![vec![DirectEvalBinding {
                    captured: false,
                    name: "z".to_string(),
                    upvalue: 2,
                    lexical: true,
                    is_const: false,
                    fn_self_name: false,
                    inner: true,
                    param: false,
                    deletable: false,
                    scope_depth: 2,
                }]],
                contains_direct_eval: true,
                source_text_range: Some((0, 18)),
                source_text_span: Some((0, 18)),
                code: code.finish(),
                spans: vec![SpanEntry {
                    pc: 0,
                    span: (0, 4),
                }],
                number_hint_sites: vec![0, 1],
                class_hint_sites: vec![ClassHintSite {
                    pc: 1,
                    class_function_id: 0,
                }],
            }],
            function_source: Some("function main() {}".to_string()),
            constants: vec![
                Constant::String {
                    utf16: vec![0xD83D, 0xDE00],
                },
                Constant::Number {
                    bits: f64::NAN.to_bits(),
                },
                Constant::FunctionId { index: 0 },
                Constant::BigInt {
                    decimal: "9007199254740993".to_string(),
                },
                Constant::RegExp {
                    pattern_utf16: vec![0x61, 0x2B],
                    flags: "gu".to_string(),
                },
            ],
            module_resolutions: vec![ModuleResolution {
                referrer: "file:///entry.ts".to_string(),
                specifier: "./other.ts".to_string(),
                attr_type: Some("xml".to_string()),
                target: "file:///other.ts".to_string(),
                deferred: true,
                dynamic: false,
                synthetic: true,
            }],
            module_inits: vec![ModuleInit {
                url: "file:///entry.ts".to_string(),
                function_id: 0,
            }],
        }
    }

    #[test]
    fn a_module_survives_a_round_trip() {
        let module = sample_module();
        let bytes = encode_module(&module);
        let restored = decode_module(&bytes).expect("round trip");
        assert_eq!(
            crate::dump::to_json_pretty(restored.module()).unwrap(),
            crate::dump::to_json_pretty(&module).unwrap()
        );
    }

    #[test]
    fn an_encoding_is_byte_stable() {
        let module = sample_module();
        assert_eq!(encode_module(&module), encode_module(&module));
    }

    #[test]
    fn bounded_encoding_never_constructs_an_oversized_result() {
        let module = sample_module();
        let expected = encode_module(&module);
        assert_eq!(
            encode_module_bounded(&module, expected.len()).unwrap(),
            expected
        );
        assert_eq!(
            encode_module_bounded(&module, expected.len() - 1).unwrap_err(),
            ModuleEncodeError::SizeLimitExceeded
        );
        assert_eq!(
            encode_module_bounded(&module, 0).unwrap_err(),
            ModuleEncodeError::SizeLimitExceeded
        );
    }

    #[test]
    fn bytes_this_encoding_did_not_write_are_rejected() {
        assert_eq!(
            decode_module(b"").unwrap_err(),
            ModuleDecodeError::MalformedEncoding
        );
        assert_eq!(
            decode_module(b"not a module at all").unwrap_err(),
            ModuleDecodeError::MalformedEncoding
        );
        let mut bytes = encode_module(&sample_module());
        bytes[0] = b'X';
        assert_eq!(
            decode_module(&bytes).unwrap_err(),
            ModuleDecodeError::MalformedEncoding
        );
    }

    #[test]
    fn a_truncated_buffer_is_rejected_at_every_length() {
        let bytes = encode_module(&sample_module());
        for end in 0..bytes.len() {
            assert!(
                matches!(
                    decode_module(&bytes[..end]),
                    Err(ModuleDecodeError::MalformedEncoding)
                ),
                "prefix of {end} bytes decoded as a module"
            );
        }
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = encode_module(&sample_module());
        bytes.push(0);
        assert_eq!(
            decode_module(&bytes).unwrap_err(),
            ModuleDecodeError::MalformedEncoding
        );
    }

    #[test]
    fn a_corrupt_length_prefix_does_not_ask_for_a_huge_allocation() {
        let module = sample_module();
        let mut bytes = encode_module(&module);
        // The count following the magic and the module string is the template
        // site count; a hostile value must be rejected against the buffer.
        let at = MAGIC.len() + 4 + module.module.len() + 1;
        bytes[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            decode_module(&bytes).unwrap_err(),
            ModuleDecodeError::MalformedEncoding
        );
    }

    #[test]
    fn a_padded_sequence_cannot_amplify_into_a_large_allocation() {
        const PADDED_COUNT: u32 = 64 * 1024;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&0u32.to_le_bytes()); // Empty module name.
        bytes.push(source_kind_tag(SourceKind::JavaScript));
        bytes.extend_from_slice(&0u32.to_le_bytes()); // No template sites.
        bytes.extend_from_slice(&PADDED_COUNT.to_le_bytes());
        bytes.resize(bytes.len() + PADDED_COUNT as usize, 0);

        let decoded = std::panic::catch_unwind(|| decode_module(&bytes))
            .expect("decoder panicked while rejecting an amplified sequence");
        assert_eq!(decoded.unwrap_err(), ModuleDecodeError::MalformedEncoding);
    }

    #[test]
    fn structurally_invalid_module_is_a_typed_decode_error() {
        let mut module = sample_module();
        module.functions[0].id = 1;
        let bytes = encode_module(&module);
        assert!(matches!(
            decode_module(&bytes),
            Err(ModuleDecodeError::Verify(
                BytecodeVerifyError::FunctionId { .. }
            ))
        ));
    }

    #[test]
    fn single_byte_mutations_never_panic_or_escape_verification() {
        let encoded = encode_module(&sample_module());
        for index in 0..encoded.len() {
            let mut mutated = encoded.clone();
            mutated[index] ^= 0xa5;
            let decoded = std::panic::catch_unwind(|| decode_module(&mutated));
            let decoded =
                decoded.unwrap_or_else(|_| panic!("decoder panicked after mutating byte {index}"));
            if let Ok(module) = decoded {
                crate::verify_module_at_base(module.module(), module.function_base())
                    .unwrap_or_else(|error| panic!("decoder admitted byte {index}: {error}"));
            }
        }
    }
}
