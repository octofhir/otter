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
//! - [`decode_module`] — flat bytes back into a module.
//!
//! # Invariants
//! - Encoding is total and decoding is fallible: any byte string that is not
//!   something [`encode_module`] produced fails to decode rather than
//!   producing a wrong module.
//! - Reads are bounds-checked. A truncated or corrupt buffer ends as `None`.
//! - The format carries no version. A reader that cannot make sense of a
//!   buffer rejects it, and the caller — which keys entries by the build that
//!   wrote them — simply produces the module again.
//! - Sequences are pre-sized from their length prefix, and the prefix is
//!   bounded by the remaining buffer, so a corrupt count cannot ask for a
//!   large allocation.
//!
//! # See also
//! - [`crate::BytecodeModule`], the value this encodes.

use crate::wordcode::{FunctionCode, INLINE_OPERAND_WORDS, Instruction};
use crate::{
    ArgumentBindingStorage, ArgumentsObjectKind, BytecodeModule, ClassHintSite, Constant,
    DirectEvalBinding, Function, MappedArgumentBinding, ModuleInit, ModuleResolution, SourceKind,
    SpanEntry, TemplateSite,
    encoding::{op_from_byte, op_to_byte},
};

/// Marks a buffer as this encoding. A buffer that does not start with it is
/// not something this module wrote.
const MAGIC: &[u8; 8] = b"otterbc\0";

/// Encode `module` as flat bytes.
#[must_use]
pub fn encode_module(module: &BytecodeModule) -> Vec<u8> {
    let mut out = Writer::new();
    out.bytes(MAGIC);
    out.string(&module.module);
    out.u8(source_kind_tag(module.source_kind));
    out.seq(&module.template_sites, Writer::template_site);
    out.seq(&module.functions, Writer::function);
    out.seq(&module.constants, Writer::constant);
    out.seq(&module.module_resolutions, Writer::module_resolution);
    out.seq(&module.module_inits, Writer::module_init);
    out.finish()
}

/// Decode flat bytes back into a module, or `None` when the bytes are not a
/// module this encoding produced.
#[must_use]
pub fn decode_module(bytes: &[u8]) -> Option<BytecodeModule> {
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
    if !input.is_at_end() {
        return None;
    }
    Some(BytecodeModule {
        module,
        template_sites,
        source_kind,
        functions,
        constants,
        module_resolutions,
        module_inits,
    })
}

struct Writer {
    out: Vec<u8>,
}

impl Writer {
    fn new() -> Self {
        Self {
            out: Vec::with_capacity(64 * 1024),
        }
    }

    fn finish(self) -> Vec<u8> {
        self.out
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }

    fn u8(&mut self, value: u8) {
        self.out.push(value);
    }

    fn bool(&mut self, value: bool) {
        self.out.push(u8::from(value));
    }

    fn u16(&mut self, value: u16) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    fn span(&mut self, span: (u32, u32)) {
        self.u32(span.0);
        self.u32(span.1);
    }

    fn string(&mut self, value: &str) {
        self.u32(value.len() as u32);
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
        self.u32(units.len() as u32);
        for unit in units {
            self.u16(*unit);
        }
    }

    fn seq<T>(&mut self, items: &[T], mut write: impl FnMut(&mut Self, &T)) {
        self.u32(items.len() as u32);
        for item in items {
            write(self, item);
        }
    }

    fn u32_seq(&mut self, items: &[u32]) {
        self.u32(items.len() as u32);
        for item in items {
            self.u32(*item);
        }
    }

    fn template_site(&mut self, site: &TemplateSite) {
        self.u32(site.cooked.len() as u32);
        for cooked in &site.cooked {
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
        self.u32(instructions.len() as u32);
        for instruction in instructions {
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
        self.optional_string(function.source_text.as_ref());
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
}

impl<'a> Reader<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self { input, at: 0 }
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

    fn string(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        let bytes = self.bytes(len)?;
        String::from_utf8(bytes.to_vec()).ok()
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
        let mut units = Vec::with_capacity(count);
        for _ in 0..count {
            units.push(self.u16()?);
        }
        Some(units)
    }

    fn seq<T>(&mut self, mut read: impl FnMut(&mut Self) -> Option<T>) -> Option<Vec<T>> {
        let count = self.count(1)?;
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            items.push(read(self)?);
        }
        Some(items)
    }

    fn u32_seq(&mut self) -> Option<Vec<u32>> {
        let count = self.count(4)?;
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            items.push(self.u32()?);
        }
        Some(items)
    }

    fn template_site(&mut self) -> Option<TemplateSite> {
        let cooked_count = self.count(1)?;
        let mut cooked = Vec::with_capacity(cooked_count);
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
        let mut instructions = Vec::with_capacity(count);
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
        let source_text = self.optional_string()?;
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
            contains_direct_eval,
            source_text,
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
                    storage: ArgumentBindingStorage::Upvalue { idx: 3 },
                }],
                module_url: "file:///entry.ts".to_string(),
                direct_eval_bindings: vec![DirectEvalBinding {
                    captured: true,
                    name: "outer".to_string(),
                    upvalue: 1,
                    lexical: true,
                    is_const: false,
                    fn_self_name: false,
                }],
                contains_direct_eval: true,
                source_text: Some("function main() {}".to_string()),
                source_text_span: Some((0, 18)),
                code: code.finish(),
                spans: vec![SpanEntry {
                    pc: 0,
                    span: (0, 4),
                }],
                number_hint_sites: vec![0, 7],
                class_hint_sites: vec![ClassHintSite {
                    pc: 1,
                    class_function_id: 0,
                }],
            }],
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
            crate::dump::to_json_pretty(&restored).unwrap(),
            crate::dump::to_json_pretty(&module).unwrap()
        );
    }

    #[test]
    fn an_encoding_is_byte_stable() {
        let module = sample_module();
        assert_eq!(encode_module(&module), encode_module(&module));
    }

    #[test]
    fn bytes_this_encoding_did_not_write_are_rejected() {
        assert!(decode_module(b"").is_none());
        assert!(decode_module(b"not a module at all").is_none());
        let mut bytes = encode_module(&sample_module());
        bytes[0] = b'X';
        assert!(decode_module(&bytes).is_none());
    }

    #[test]
    fn a_truncated_buffer_is_rejected_at_every_length() {
        let bytes = encode_module(&sample_module());
        for end in 0..bytes.len() {
            assert!(
                decode_module(&bytes[..end]).is_none(),
                "prefix of {end} bytes decoded as a module"
            );
        }
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = encode_module(&sample_module());
        bytes.push(0);
        assert!(decode_module(&bytes).is_none());
    }

    #[test]
    fn a_corrupt_length_prefix_does_not_ask_for_a_huge_allocation() {
        let module = sample_module();
        let mut bytes = encode_module(&module);
        // The count following the magic and the module string is the template
        // site count; a hostile value must be rejected against the buffer.
        let at = MAGIC.len() + 4 + module.module.len() + 1;
        bytes[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_module(&bytes).is_none());
    }
}
