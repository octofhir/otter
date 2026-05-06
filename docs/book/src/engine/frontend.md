# Frontend And Compilation

Otter parses JavaScript and TypeScript through OXC. The active frontend stack is:

- `crates-next/otter-syntax`: source kind detection, OXC parse options, and parse-once callbacks.
- `crates-next/otter-compiler`: AST-to-bytecode lowering and TypeScript erasure.
- `crates-next/otter-bytecode`: bytecode module, disassembly, and JSON dump formats.

Do not regex-parse JavaScript or TypeScript source. Consumers that need to inspect
module syntax or other AST properties should use `otter_syntax::with_program` and
reuse the parsed OXC program when compiling script sources.

The foundation TypeScript policy is:

- erase type-only constructs such as `interface`, `type`, `declare`, `import type`,
  `export type`, abstract methods, `as`, `satisfies`, non-null assertions, and type
  instantiation syntax;
- reject runtime TypeScript constructs that cannot be erased cleanly in the current
  engine slice, including `enum`, runtime `namespace`, and decorators;
- preserve original source spans through diagnostics and stack traces.

Bytecode dumps are part of the supported CLI/debugging surface:

```bash
otter --dump-bytecode path/to/script.js
otter --dump-bytecode=json path/to/script.ts
```

The text dump starts with:

```text
; otter bytecode dump v1 - module=<specifier> source_kind=<javascript|typescript>
```

The JSON dump is intended for tools and tests. Keep it stable when possible; when
the schema must change, update tests, docs, and any downstream consumers in the
same patch.
