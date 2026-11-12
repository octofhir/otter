---
title: "Frontend And Compilation"
---

Otter parses JavaScript and TypeScript through OXC. The active frontend stack is:

- `crates/otter-syntax`: source kind detection, OXC parse options, and parse-once callbacks.
- `crates/otter-compiler`: AST-to-bytecode lowering and TypeScript erasure.
- `crates/otter-bytecode`: bytecode module, disassembly, and JSON dump formats.

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
; otter bytecode dump — module=<specifier> source_kind=<javascript|typescript>
```

The JSON dump is intended for tools and tests. It has one current shape: change
the format in place and update tests, docs, and downstream consumers in the same
patch.

## Строки AST и граница загрузчика

OXC `JSStr` хранит JavaScript-строку, которая может содержать непарный суррогат.
`as_str()` возвращает `None` для такой строки. Не используйте `display()` или
lossy-конвертацию для значений, имён свойств и ключей кэша: они теряют различия
между строками. При lowering используйте точные code units из `encode_utf16()`.

`TemplateSite.cooked` содержит `Vec<Option<Vec<u16>>>`: массив UTF-16 для каждого
cooked-сегмента и `None` для недопустимого escape в tagged template. `raw` хранит
исходное UTF-8 написание. Binary codec и JSON dump используют этот единственный
текущий формат; совместимого чтения старых артефактов нет. VM создаёт cooked-строки
в handle scope, затем замораживает и кэширует template arrays по прежним правилам.

Host module requests по-прежнему используют Rust `String`. Общие функции
`otter_syntax::module_literal_text` и `module_attribute_type` проверяют эту границу
в compiler и loader: непарный суррогат в статическом specifier или import attribute
даёт диагностику, а не подменённое имя. Динамический import с такой строкой не
попадает в предварительный сбор зависимостей. VM выполняет `ToString` и чтение
options, затем отвергает такой запрос через Promise с `TypeError` до поиска
в module map и вызова host loader. То же правило действует для `type` attribute.
Корректные surrogate pairs и настоящий U+FFFD остаются допустимыми именами.

Сначала читаются все enumerable string-keyed attributes, затем проверяются их
типы и поддержка host. Значения остаются в handle scope: следующий getter или
Proxy trap может вызвать GC. Ошибка getter имеет приоритет над ошибкой проверки
ранее прочитанного attribute; ошибка `ToString` возникает до чтения options.
Это порядок [EvaluateImportCall](https://tc39.es/ecma262/#sec-evaluate-import-call),
а запрет непарных суррогатов — ограничение UTF-8 host API Otter, не JS-строк вообще.

Поддерживается только ключ `type`. Compiler и loader проверяют весь static
`with` clause через `module_attribute_type`, включая ключи после `type`, до
преобразования AST в `(specifier, attr_type)`. Неизвестный ключ даёт `SyntaxError`
при загрузке static request; неизвестный ключ в options самого `import()` даёт
`TypeError`. Если `import()` загружает модуль с неверным static request, Promise
отклоняется с `SyntaxError`, а не с `TypeError`.

Async prefetch не решает судьбу parse/request diagnostics: основной построитель
графа знает, обязателен ли модуль или это лишь цель `import()`. Он сохраняет
ошибку dynamic target до выполнения вызова. Ошибки I/O и ресурсов в этом шаге
не менялись. `type` values и правила выбора JSON/XML/других data formats остаются
ответственностью loader, не синтаксического helper.

## Проверка RegExp без runtime matcher

Compiler проверяет literals через `otter_regex::validate_utf16`, а не через
`Regex::compile_str`. Проверка и runtime-компиляция используют один parser,
одинаковые early errors и предел вложенности. Compiler не строит executable IR
и повторно не кодирует pattern: проверенный UTF-16 идёт в bytecode constant.

Workspace dependency сохраняет `unicode`, но отключает default features.
VM явно включает `runtime`: это matcher, IR lowering и обе native-архитектуры.
Прямая default-сборка `otter-regex` также сохраняет весь runtime. Host compiler
и build scripts Node/Web не должны включать `runtime` ради проверки literals.
AST остаётся внутренним; наружу validation возвращает только `Result<(), RegexError>`.

Проверка границы:

```sh
cargo test -p otter-regex --no-default-features --features unicode
cargo test -p otter-regex -p otter-compiler
cargo tree -p otter-compiler -e normal,build
```
