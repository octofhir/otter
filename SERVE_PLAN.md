# Otter.serve + Hono: находки и план

Дата: 2026-07-07. Ветка: `refactor/vm-api-structure` (в дереве есть чужие
незакоммиченные правки otter-jit/otter-vm — не трогаю).

## 1. Главные находки

### 1.1 `Otter.serve` не существует
- Полная реализация (hyper, WebSocket, TLS, pub/sub, Bun-parity API) жила в
  legacy-крейте `otter-engine` и удалена целиком в `daf8864c` вместе со старым
  движком. Порт невозможен — старый op-registry механизм мёртв.
- Осталась сирота: `packages/otter-types/serve.d.ts` (195 строк) — типы
  публикуются для API, которого нет. `crates/otter-pm/src/types/otter/`
  (источник правды по CLAUDE.md) удалён в `a65cf685` — CLAUDE.md устарел.
- Проверено бинарём: `typeof Otter === "undefined"`.

### 1.2 Hono на otter — блокеры (в порядке обнаружения)
Модульный резолв из `node_modules` работает: hono dist грузится и парсится.

| # | Блокер | Статус |
|---|--------|--------|
| 1 | `.ts` без `"type":"module"` в package.json парсится как script (import/TLA падают) | открыт (мелочь) |
| 2 | Компилятор: `this.#order = ++this.#order` → `FEATURE_NOT_IN_SLICE: UpdateExpression on non-identifier operand` (hono trie-router) | открыт |
| 3 | Request/Response/Headers не стандартные: init игнорируется, нет body-методов, прототип оторван, `instanceof` false | **исправлено** (см. 2.1) |
| 4 | **Engine-баг**: async-функция, возвращающая promise, не адоптит thenable — `outer()` резолвится самим promise, а не его значением | найден корень (см. 2.2) |
| 5 | URL: `searchParams` отсутствует, прототип-цепочка оторвана (`instanceof URL` false) | открыт, hono может не задеть |
| 6 | `console.log([1,2])` печатает `[object Array]` | косметика |

Причина #3/#5 общая: инстансы native-классов строятся `ObjectBuilder::from_host_data`
с копией методов на каждом объекте, мимо прототипа класса (`couch!` создаёт
prototype, но конструкторы его не подвешивают).

### 1.3 Конкурент (тот самый репо)
- Его serve — игрушка: свой HTTP/1.1 на `std::net`, **один accept одновременно**
  (сериальная обработка), `Connection: close` на каждый ответ, буферизованные
  body, без TLS/WS/keep-alive. Опубликованная цифра только cold start ~15.8ms
  (Bun ~16.3, Node ~53.9) — за счёт precompiled-AST снапшота JS-глю.
  Throughput не публикует (нечем хвастаться).
- Но hono-пример у него **работает** — впереди фактом, не качеством.
- Extension-механизм: data-driven дескриптор
  `{name, globals, namespaces, state_init(typed slot), js_init, js_init_snapshot}`
  + один `install(engine, &[ext])`. Стоит взять две идеи:
  1. **build-time precompile JS-глю** (build.rs склеивает js → компилирует
     снапшот, рантайм декодирует вместо re-parse) — его cold-start win;
  2. **typed per-extension state slot** (дено-подобный OpState).

## 2. Что уже сделано в этой сессии (закоммичено не было)

### 2.1 `web_fetch.js` — стандартные Headers/Request/Response в JS
- Новый `crates/otter-web/src/web_fetch.js` (~700 строк): WHATWG Headers
  (валидация, combine+sort итерация, getSetCookie, guard), body mixin
  (`text/json/arrayBuffer/bytes/blob/formData`, `body`-стрим поверх
  ReadableStream, bodyUsed), Request (стандартный `(input, init)`, clone,
  method normalize/forbidden), Response (стандартный `(body, init)`,
  `Response.json/redirect/error`, ok/clone), multipart+urlencoded formData
  парсер. Плюс скрытая фабрика `__otterFetchInternals`
  (makeRequest/responseParts/collectStream) — хот-пас для будущего serve без
  валидаций конструктора.
- Native `request_response.rs` и `headers.rs` удалены; `WEB_API_CLASSES` теперь
  `[URL, Blob]`; shim встроен в lazy-global механизм (третий источник после
  web_bootstrap/web_streams).
- Тесты `crates/otter-web/tests/web.rs` переписаны: **9/12 зелёные**, 2 фейла
  были ошибками ожиданий теста (исправить тривиально), 1 — engine-баг ниже.

### 2.2 Engine-баг: async return не резолвит thenable
- Изолированный репро: `.then`-флэттенинг работает, `new Promise(r => r(p))`
  работает, `async () => somePromise()` — НЕ работает.
- Корень: `Interpreter::pop_frame` (`crates/otter-vm/src/interp/frames.rs:396`)
  для async-фрейма делает `result_promise.fulfill(value)` напрямую — по споке
  (AsyncFunctionStart → promiseCapability.[[Resolve]]) возвращённый thenable
  обязан адоптиться.
- Фикс: в async-ветке pop_frame гнать значение через resolve-семантику
  (native promise → adoption-handlers + attach_then; user thenable → thenable
  job; прочее → fulfill). Вся машинерия уже есть в `promise_dispatch.rs`
  (resolve_native_body, make_resolve_adoption_handlers), нужен interp-level
  вход без NativeCtx. Не начат — прерван на этом месте.
- Замечено попутно (НЕ трогал): adoption-handlers захватывают
  `JsPromiseHandle` move'ом в замыкание, хотя doc-comment `settle_native_promise`
  прямо объясняет, почему handle надо перечитывать из traced captures —
  потенциальный use-after-move под GC-stress.

## 3. Архитектура Otter.serve (предлагаемая)

Разведка plumbing (полный отчёт получен, факты проверены агентом по коду):

- Async-мост native→promise: есть токен-реестр
  `Runtime::register_pending_promise()` + `RuntimeHandle::settle_promise()`
  (inbox-сообщение), но payload узкий (`HostSettleOutcome` = примитивы) и из
  `NativeCtx` реестр НЕДОСТУПЕН — ни одного потребителя в дереве.
- Единственный прецедент внешних событий → JS: worker-модуль на **1ms
  setInterval поллинге** (`worker.rs`) — для сервера неприемлемо.
- Event loop: isolate-тред с mpsc inbox (`RuntimeMessage`, приватный enum);
  таймеры через `TimerScheduler` → Tokio sleep → inbox. Keep-alive процесса:
  `RuntimeLiveness::Ref/Unref` счётчики — сервер держит один Ref на время
  жизни листенера.
- Вызов JS из Rust на isolate-треде: `run_callable_sync`; байты:
  `array_buffer_from_bytes_rooted` + `ctx.construct(Uint8Array)`.
- Капабилити: `lodge! { capabilities = true }` даёт `&CapabilitySet`;
  гейт `caps.net.matches("host:port")` до bind.
- `HOSTED_MODULES` в otter-modules НЕ подключены в CLI (только
  with_node_apis/with_web_apis) — подключить по образцу `NodeApiBuilderExt`.
- Hosted-специфаер = точная строка, значит модуль с именем `otter` (для
  `import {serve} from 'otter'`) легален (прецедент: bare `fs`, `assert`).
- CLAUDE.md врёт про макросы: `#[dive]`/`burrow!` не существуют; есть
  `holt!`/`couch!`/`lodge!`/`raft!` + derive Pelt/Groom.

Дизайн (v1, производство сразу):
1. **Крейт-место**: `crates/otter-modules` (по решению юзера), hyper 1.x +
   tokio + http-body-util. Fetch-классы остаются в otter-web (правило CLAUDE.md),
   обмен через `__otterFetchInternals` — плоские данные, ноль JS-объектов
   в Rust-стейте.
2. **Дispatch без поллинга**: новый механизм доставки "вызови JS-колбэк с
   owned-данными" в isolate-inbox (расширение RuntimeMessage или
   interp-side callback registry по образцу TimerCallbacks). Это и есть
   недостающий кирпич — он же пригодится fetch/WS/fs-watch.
3. **Пер-запрос путь**: hyper Service на tokio-воркере → owned
   `{method, url, headers flat, body bytes}` + oneshot →
   inbox → на isolate-треде `__otterFetchInternals.makeRequest(...)` →
   handler(request) → await → `responseParts()` → oneshot → hyper строит
   ответ. Body-строки не конвертируются в JS (в Rust один раз в байты).
   Keep-alive из коробки (hyper auto builder), HTTP/1.1+h2c.
4. **API**: `Otter.serve(opts)` глобал + `import {serve} from 'otter'` —
   один и тот же native, opts Bun-совместимые (fetch/port/hostname/error/
   onListen/idleTimeout), возврат `Server{stop, ref/unref, port, hostname, url,
   fetch}`. WebSocket/TLS — v2 (типы уже есть в serve.d.ts).
5. **Идея-снапшот**: v1 — прекомпиляция всех JS-шимов (web_bootstrap/streams/
   fetch + серверный глю) в bytecode на build.rs, рантайм декодирует
   (cold-start). Отдельный слайс, не блокирует serve.
6. **Идея-typed state**: типизированный state-slot для hosted-модулей
   (ServerRegistry живёт там, не в глобалах/thread_local — thread_local
   запрещён политикой).

## 4. Порядок работ (остаток)

1. Дочинить 2 тестовых ожидания web.rs (тривиально).
2. Engine-фикс async-return thenable adoption (2.2) + unit-тест.
3. Компилятор: UpdateExpression на member-операнде (`++obj.x`, `++this.#f`,
   `obj.x--`…) — блокер hono trie-router.
4. Serve v1 по дизайну §3 (включая dispatch-механизм и Ref-liveness).
5. Модуль `otter` (`import {serve} from 'otter'`) + глобал `Otter.serve`.
6. Hono end-to-end + smoke-бенч (wrk/hey) против node/bun того же приложения.
7. `.ts`-как-модуль в CLI; URL searchParams+прототип; console.log массивов.
8. Снапшот-прекомпиляция шимов (cold start).
9. Обновить/убрать сироту `packages/otter-types/serve.d.ts` под реальный API;
   поправить CLAUDE.md (typos: макросы, пути типов).

## 5. Решения, которые нужны от тебя

1. **Scope v1 serve**: HTTP/1.1 + h2c keep-alive, без TLS/WS — ок? (TLS/WS v2)
2. **Dispatch**: расширяем приватный `RuntimeMessage` inbox (чисто, но трогает
   otter-runtime ядро) или callback-registry поверх существующего
   таймер-механизма (меньше касаний, чуть кривее)? Рекомендую inbox.
3. **Global `Otter` namespace**: вводим (`Otter.serve`, потом Otter.file и
   т.п., Bun-стиль) или только модульный `import {serve} from 'otter'`?
   Рекомендую оба, реализация одна.
4. GC-подозрение в adoption-handlers (2.2, use-after-move) — чинить сразу в
   том же PR или отдельным тикетом с GC_STRESS репро?

## 6. Решения и старт реализации 2026-07-07

Принято:

1. `serve` живёт в `crates/otter-modules`, потому что это Otter-specific API.
   `otter-web` остаётся владельцем Fetch-классов (`Request`/`Response`/`Headers`).
2. v1 scope: HTTP/1.1 + keep-alive/h2c, без TLS/WebSocket. TLS/WS — v2.
3. Dispatch делаем через isolate inbox/runtime boundary, без polling.
4. Публичная форма — обе: `import { serve } from "otter"` и `Otter.serve`.
5. Node-модули добираются параллельно по фактическим SSR/Hono/Vite блокерам.

Сделано первым slice:

- `lodge!` расширен exact `specifier = "otter"` для bare hosted modules.
- Добавлен `crates/otter-modules/src/serve.rs` с hosted module `"otter"` и
  глобальным installer для `Otter.serve`.
- Добавлен `OtterModulesBuilderExt::with_otter_modules()`.
- CLI подключает `with_otter_modules()` вместе с Node/Web APIs.
- `Runtime::install_native_global_call` открыт как public API для captured
  native globals (нужно, чтобы `Otter.serve` захватывал capability snapshot).
- Добавлен временный smoke-transport в `serve.rs`: `TcpListener` + sync
  `fetch(Request) -> Response`, построение `Request` и разбор `Response` через
  `__otterFetchInternals`.
- Важно: этот smoke-transport блокирует native call и НЕ является целевой
  архитектурой. Он нужен только для проверки Fetch-контракта и CLI plumbing.

Следующий обязательный slice перед benchmark:

1. Добавить в `otter-runtime` общий long-lived host resource primitive:
   `RuntimeLiveness::Ref/Unref` token, который держит event loop живым до
   явного `close()`/drop, а не до завершения одного host op.
2. Добавить host-event delivery в isolate inbox: owned request data + oneshot
   reply, callback выполняется на isolate thread, без polling и без блокировки
   native call.
3. Перевести `Otter.serve` на nonblocking return: `serve()` сразу возвращает
   server object (`close`, затем `ref/unref`), accept/read/write живут на
   Tokio/Hyper worker, JS handler вызывается через inbox.
4. Использовать тот же primitive для будущего `node:net`, чтобы TCP servers и
   Otter HTTP servers одинаково держали процесс живым.

Проверено:

- `cargo test -p otter-modules` — 9 passed.
- `cargo run -p otter-cli -- /tmp/otter-serve-api-smoke.mjs` вывел:
  `function` и `object function`.
- `cargo run -p otter-cli -- run /tmp/otter-serve-response-smoke.mjs
  --allow-net=127.0.0.1:34567 --timeout 0` дошёл до `Otter.serve listening on
  http://127.0.0.1:34567` при запуске вне sandbox.
