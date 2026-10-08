//! Encoding a validated module as a WebAssembly binary.
//!
//! # Invariants
//! - Function imports precede the defined functions: the `ToInt32` helper,
//!   then the asm.js functions in declaration order, so buffered call
//!   targets resolve only here.
//! - The heap is the module's own memory `0`, exactly the heap's length.

use super::*;

impl<'a> ModuleTranslator<'a> {
    // -- Encoding -----------------------------------------------------------

    pub(super) fn finish(mut self) -> Result<Translation> {
        let Some(exports) = self.exports.take() else {
            return fail("module without exports");
        };
        let mut signatures = Vec::with_capacity(self.functions.len());
        for info in &self.functions {
            let Some(signature) = info.signature.clone() else {
                return fail("function without type");
            };
            signatures.push(signature);
        }
        // Defined functions: the helpers, then the asm.js functions.
        let mut function_types = vec![self.type_of(vec![ValType::F64], vec![ValType::I32])];
        for signature in &signatures {
            function_types.push(self.signature_type(signature));
        }
        let Self {
            heap,
            types,
            foreign,
            global_imports,
            globals,
            function_imports,
            functions,
            bodies,
            tables,
            table_elements,
            stdlib,
            memory,
            ..
        } = self;
        let defined_base = function_imports.len() as u32;

        let mut module = WasmModule::new();
        let mut type_section = TypeSection::new();
        for (params, results) in &types {
            type_section
                .ty()
                .function(params.iter().copied(), results.iter().copied());
        }
        module.section(&type_section);

        let mut imports = ImportSection::new();
        let mut linked = Vec::with_capacity(global_imports.len() + function_imports.len());
        for (member, ty) in global_imports {
            imports.import(
                "",
                &format!("g{}", linked.len()),
                EntityType::Global(GlobalType {
                    val_type: ty,
                    mutable: false,
                    shared: false,
                }),
            );
            linked.push(Import::Value {
                foreign: member,
                double: ty == ValType::F64,
            });
        }
        for (import, ty) in function_imports {
            imports.import("", &format!("f{}", linked.len()), EntityType::Function(ty));
            linked.push(import);
        }
        module.section(&imports);

        let mut function_section = FunctionSection::new();
        for ty in &function_types {
            function_section.function(*ty);
        }
        module.section(&function_section);

        let table_len: u32 = tables.iter().map(|table| table.mask + 1).sum();
        if table_len > 0 {
            let mut table_section = TableSection::new();
            table_section.table(TableType {
                element_type: RefType::FUNCREF,
                table64: false,
                minimum: u64::from(table_len),
                maximum: Some(u64::from(table_len)),
                shared: false,
            });
            module.section(&table_section);
        }

        // The heap is a defined memory: Wasmtime builds a module's own
        // memories through the engine's memory creator, which aliases the
        // heap buffer.
        if memory {
            let Some(heap) = heap else {
                return fail("module views the heap but has none");
            };
            let mut memory_section = MemorySection::new();
            memory_section.memory(heap_memory_type(heap.len));
            module.section(&memory_section);
        }

        let mut global_section = GlobalSection::new();
        for (ty, init) in &globals {
            global_section.global(
                GlobalType {
                    val_type: *ty,
                    mutable: true,
                    shared: false,
                },
                init,
            );
        }
        module.section(&global_section);

        let mut export_section = ExportSection::new();
        let mut exported: Vec<ExportedFunction> = Vec::new();
        let export_names: Vec<&String> = match &exports {
            Exports::Single(index) => vec![index],
            Exports::Object(entries) => entries.iter().map(|(_, index)| index).collect(),
        };
        for index in export_names {
            if exported.iter().any(|function| &function.export == index) {
                continue;
            }
            let id: usize = index.parse().expect("export names are function indices");
            export_section.export(
                index,
                ExportKind::Func,
                defined_base + HELPER_COUNT + id as u32,
            );
            exported.push(ExportedFunction {
                export: index.clone(),
                name: functions[id].name.to_string(),
                arity: u8::try_from(signatures[id].params.len()).unwrap_or(u8::MAX),
            });
        }
        module.section(&export_section);

        if table_len > 0 {
            let mut element_section = ElementSection::new();
            let entries: Vec<u32> = table_elements
                .iter()
                .map(|element| defined_base + element)
                .collect();
            element_section.active(
                Some(0),
                &ConstExpr::i32_const(0),
                Elements::Functions(Cow::Owned(entries)),
            );
            module.section(&element_section);
        }

        let mut code = CodeSection::new();
        code.function(&to_int32_slow_body());
        for body in bodies {
            let Some((locals, ops)) = body else {
                return fail("function without body");
            };
            let mut function = WasmFunction::new_with_locals_types(locals);
            for op in ops {
                match op {
                    Op::Wasm(instruction) => function.instruction(&instruction),
                    Op::Call(Callee::Import(index)) => {
                        function.instruction(&Instruction::Call(index))
                    }
                    Op::Call(Callee::Defined(index)) => {
                        function.instruction(&Instruction::Call(defined_base + index))
                    }
                };
            }
            function.instruction(&Instruction::End);
            code.function(&function);
        }
        module.section(&code);

        Ok(Translation {
            wasm: module.finish(),
            foreign,
            imports: linked,
            stdlib,
            memory,
            exports,
            functions: exported,
        })
    }
}

/// The asm.js heap as wasm memory: the exact byte length, in 64 KiB pages
/// when it divides evenly and in one-byte pages otherwise.
fn heap_memory_type(len: u32) -> MemoryType {
    let (pages, page_size_log2) = if len % 65536 == 0 {
        (u64::from(len / 65536), None)
    } else {
        (u64::from(len), Some(0))
    };
    MemoryType {
        minimum: pages,
        maximum: Some(pages),
        memory64: false,
        shared: false,
        page_size_log2,
    }
}

/// `(func (param f64) (result i32))` computing JavaScript `ToInt32` of a
/// double outside the fast `|x| < 2^63` path: NaN and infinities give `0`;
/// larger finite values reduce exactly modulo 2^32 (every such double is a
/// multiple of 2^11, so the remainder is exact).
fn to_int32_slow_body() -> WasmFunction {
    let mut function = WasmFunction::new_with_locals_types([]);
    for instruction in [
        Instruction::LocalGet(0),
        Instruction::LocalGet(0),
        Instruction::F64Const(4_294_967_296.0f64.into()),
        Instruction::F64Div,
        Instruction::F64Trunc,
        Instruction::F64Const(4_294_967_296.0f64.into()),
        Instruction::F64Mul,
        Instruction::F64Sub,
        Instruction::I64TruncSatF64S,
        Instruction::I32WrapI64,
        Instruction::End,
    ] {
        function.instruction(&instruction);
    }
    function
}
