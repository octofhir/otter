use otter_runtime::{Runtime, SourceInput};
use otter_web::blob::Blob;
use otter_web::url::WebUrl;
use otter_web::{WebApiBuilderExt, web_api_classes};

fn eval_string(runtime: &mut Runtime, source: &str) -> String {
    runtime
        .eval(SourceInput::from_javascript(source))
        .unwrap()
        .completion_string()
        .to_string()
}

#[test]
fn web_api_specs_are_static_and_ordered() {
    let specs = web_api_classes();
    assert_eq!(
        specs.iter().map(|spec| spec.name()).collect::<Vec<_>>(),
        [
            "URL",
            "Blob",
            "File",
            "crypto",
            "WebAssembly.Module",
            "WebAssembly.Memory",
            "WebAssembly.Global",
            "WebAssembly.Table",
            "WebAssembly.Tag",
            "WebAssembly.Exception",
            "WebAssembly",
        ]
    );
}

#[test]
fn url_parses_and_mutates_parts() {
    let base = WebUrl::parse("https://example.com/root/", None).unwrap();
    let mut url = WebUrl::parse("../a?x=1#top", Some(&base)).unwrap();
    assert_eq!(url.href(), "https://example.com/a?x=1#top");
    assert_eq!(url.protocol(), "https:");
    assert_eq!(url.origin(), "https://example.com");
    url.set_pathname("/b");
    url.set_search("?q=otter");
    url.set_hash("#done");
    assert_eq!(url.href(), "https://example.com/b?q=otter#done");
}

#[test]
fn blob_slices_and_decodes_text() {
    let blob = Blob::new(b"hello world".to_vec(), "TEXT/PLAIN");
    assert_eq!(blob.size(), 11);
    assert_eq!(blob.content_type(), "text/plain");
    assert_eq!(blob.slice(6, None, None).text(), "world");
}

#[test]
fn blob_constructor_assembles_parts_and_async_reads() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        globalThis.out = "pending";
        // Mixed BlobParts: string, typed-array bytes, ArrayBuffer, nested Blob.
        const inner = new Blob(["cd"]);
        const parts = ["ab", new Uint8Array([0x2d]), new Uint8Array([0x65, 0x66]).buffer, inner];
        const blob = new Blob(parts, { type: "Text/Plain" });
        let sync = blob.size + "|" + blob.type;
        const ab = blob.arrayBuffer();
        sync += "|" + (typeof ab.then === "function");
        ab.then((buffer) => {
          globalThis.out = sync + "|" + (buffer instanceof ArrayBuffer) + "|" + buffer.byteLength
            + "|" + new TextDecoder().decode(new Uint8Array(buffer));
        }, (err) => { globalThis.out = "ERR:" + err; });
        sync
        "#,
    );
    // Synchronous portion: size, normalized type, and a thenable arrayBuffer().
    assert_eq!(result, "7|text/plain|true");
    // "ab" + "-" + "ef" + "cd" = "ab-efcd" (7 bytes), read after microtasks drain.
    let after = eval_string(&mut runtime, "out");
    assert_eq!(after, "7|text/plain|true|true|7|ab-efcd");
}

#[test]
fn native_host_instances_link_class_prototype() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        var out = "";
        const b = new Blob(["hi"]);
        out += (b instanceof Blob) + "|";
        out += (Object.getPrototypeOf(b) === Blob.prototype) + "|";
        // Instance carries only host data; size/type/methods come from the prototype.
        out += (Object.getOwnPropertyNames(b).length === 0) + "|";
        out += (b.size === 2) + "|" + (typeof b.arrayBuffer === "function") + "|";
        // File honors new.target: instanceof File AND Blob.
        const f = new File(["x"], "n.txt");
        out += (f instanceof File) + "|" + (f instanceof Blob) + "|" + (f.name === "n.txt") + "|";
        // URL links its prototype too.
        const u = new URL("https://e.com/p");
        out += (u instanceof URL) + "|" + (u.href === "https://e.com/p");
        out
        "#,
    );
    assert_eq!(result, "true|true|true|true|true|true|true|true|true|true");
}

#[test]
fn subtle_crypto_hmac_aes_gcm_and_pbkdf2() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        globalThis.out = "pending";
        (async () => {
          const enc = new TextEncoder();
          const data = enc.encode("message");
          // HMAC sign / verify + raw export round-trip.
          const key = await crypto.subtle.importKey(
            "raw", enc.encode("secret"), { name: "HMAC", hash: "SHA-256" }, true, ["sign", "verify"]);
          const sig = await crypto.subtle.sign("HMAC", key, data);
          const good = await crypto.subtle.verify("HMAC", key, sig, data);
          const bad = await crypto.subtle.verify("HMAC", key, sig, enc.encode("tampered"));
          const exported = new Uint8Array(await crypto.subtle.exportKey("raw", key));
          const hmac = sig.byteLength + ":" + good + ":" + bad + ":" + new TextDecoder().decode(exported);
          // AES-GCM encrypt / decrypt round-trip.
          const aes = await crypto.subtle.generateKey({ name: "AES-GCM", length: 256 }, true, ["encrypt", "decrypt"]);
          const iv = crypto.getRandomValues(new Uint8Array(12));
          const ct = await crypto.subtle.encrypt({ name: "AES-GCM", iv }, aes, data);
          const pt = await crypto.subtle.decrypt({ name: "AES-GCM", iv }, aes, ct);
          const aesOut = (ct.byteLength > data.byteLength) + ":" + new TextDecoder().decode(pt);
          // PBKDF2 deriveBits.
          const base = await crypto.subtle.importKey("raw", enc.encode("password"), "PBKDF2", false, ["deriveBits"]);
          const bits = await crypto.subtle.deriveBits(
            { name: "PBKDF2", hash: "SHA-256", salt: new Uint8Array([1,2,3,4]), iterations: 1000 }, base, 256);
          globalThis.out = hmac + "|" + aesOut + "|" + bits.byteLength;
        })().catch((e) => { globalThis.out = "ERR:" + e.name + ":" + e.message; });
        "pending"
        "#,
    );
    assert_eq!(result, "pending");
    let after = eval_string(&mut runtime, "out");
    assert_eq!(after, "32:true:false:secret|true:message|32");
}

#[test]
fn byte_stream_serves_default_and_byob_reads() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        globalThis.out = "pending";
        const stream = new ReadableStream({
          type: "bytes",
          start(controller) {
            controller.enqueue(new Uint8Array([1, 2, 3]));
            controller.enqueue(new Uint8Array([4, 5]));
            controller.close();
          },
        });
        // A byte stream's default reader yields Uint8Array chunks.
        const sync = (stream instanceof ReadableStream) + "";
        const defaultReader = stream.getReader();
        (async () => {
          const a = await defaultReader.read();
          const first = (a.value instanceof Uint8Array) + ":" + Array.from(a.value).join(",");
          defaultReader.releaseLock();
          // A BYOB reader copies bytes into the caller's view.
          const byob = stream.getReader({ mode: "byob" });
          const r = await byob.read(new Uint8Array(8));
          const second = Array.from(r.value).join(",") + ":" + r.value.byteLength;
          const end = await byob.read(new Uint8Array(8));
          globalThis.out = sync + "|" + first + "|" + second + "|" + end.done;
        })();
        sync
        "#,
    );
    assert_eq!(result, "true");
    let after = eval_string(&mut runtime, "out");
    // default read → [1,2,3]; BYOB read → [4,5] (2 bytes); then done.
    assert_eq!(after, "true|true:1,2,3|4,5:2|true");
}

#[test]
fn url_pattern_matches_named_groups_and_wildcards() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        var out = "";
        const p = new URLPattern({ pathname: "/books/:id" });
        out += (p instanceof URLPattern) + "|";
        out += p.test("https://example.com/books/42") + "|";
        out += p.test("https://example.com/books/42/extra") + "|";
        const r = p.exec("https://example.com/books/42");
        out += r.pathname.groups.id + "|";
        // Wildcard + protocol/hostname patterns.
        const q = new URLPattern("https://*.example.com/api/*");
        out += q.test("https://a.example.com/api/users") + "|";
        out += q.test("https://a.other.com/api/users") + "|";
        // Constructor-string pathname group, matched via an init input
        // (a bare relative string is not a valid URL for exec).
        const s = new URLPattern("/files/:name.:ext");
        const m = s.exec({ pathname: "/files/report.pdf" });
        out += m.pathname.groups.name + "." + m.pathname.groups.ext + "|";
        out += (s.exec({ pathname: "/files/nope" }) === null);
        out
        "#,
    );
    assert_eq!(result, "true|true|false|42|true|false|report.pdf|true");
}

#[test]
fn singleton_web_class_globals_are_branded_and_unconstructable() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        var out = "";
        const tag = (v) => Object.prototype.toString.call(v);
        // `crypto` is an instance of `Crypto`, methods live on the prototype.
        out += (crypto instanceof Crypto) + "|";
        out += (tag(crypto) === "[object Crypto]") + "|";
        out += (typeof Crypto.prototype.getRandomValues === "function") + "|";
        out += (typeof Crypto.prototype.randomUUID === "function") + "|";
        out += (crypto.subtle instanceof SubtleCrypto) + "|";
        out += (tag(crypto.subtle) === "[object SubtleCrypto]") + "|";
        out += (typeof SubtleCrypto.prototype.digest === "function") + "|";
        out += (typeof CryptoKey === "function") + "|";
        // `performance`/`navigator` are instances of their branded classes.
        out += (performance instanceof Performance) + "|";
        out += (tag(performance) === "[object Performance]") + "|";
        out += (navigator instanceof Navigator) + "|";
        out += (tag(navigator) === "[object Navigator]") + "|";
        out += (navigator.userAgent.startsWith("Otter/")) + "|";
        // None of the interfaces are directly constructable.
        const illegal = (fn) => {
          try { fn(); return "ran"; } catch (e) { return e instanceof TypeError; }
        };
        out += illegal(() => new Crypto()) + "|";
        out += illegal(() => new SubtleCrypto()) + "|";
        out += illegal(() => new CryptoKey()) + "|";
        out += illegal(() => new Performance()) + "|";
        out += illegal(() => new Navigator());
        // Functional smoke: randomUUID + digest still work through the prototype.
        out += "|" + (crypto.randomUUID().length === 36);
        out
        "#,
    );
    assert_eq!(
        result,
        "true|true|true|true|true|true|true|true|true|true|true|true|true|true|true|true|true|true|true"
    );
}

#[test]
fn transform_stream_uses_branded_default_controller() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        globalThis.out = "pending";
        let seenController;
        const ts = new TransformStream({
          start(c) { seenController = c; },
          transform(chunk, c) { c.enqueue(chunk * 2); },
        });
        // Not directly constructable — checkable synchronously.
        var ctor = "";
        try { new TransformStreamDefaultController(); ctor = "ran"; }
        catch (e) { ctor = String(e instanceof TypeError); }

        const writer = ts.writable.getWriter();
        const reader = ts.readable.getReader();
        const collected = [];
        (async () => {
          writer.write(1); writer.write(2); writer.write(3);
          writer.close();
          while (true) {
            const { value, done } = await reader.read();
            if (done) break;
            collected.push(value);
          }
          // The controller the transformer received is a branded instance;
          // its start ran once the writable side pulled, so check it now.
          let brand = (seenController instanceof TransformStreamDefaultController) + "|";
          brand += (Object.prototype.toString.call(seenController) === "[object TransformStreamDefaultController]") + "|";
          brand += (typeof seenController.enqueue === "function") + "|";
          brand += (typeof TransformStreamDefaultController.prototype.terminate === "function");
          globalThis.out = ctor + "|" + brand + "||" + collected.join(",");
        })();
        ctor
        "#,
    );
    assert_eq!(result, "true");
    let after = eval_string(&mut runtime, "out");
    // ctor-throws | branding checks || each written value doubled through the stream.
    assert_eq!(after, "true|true|true|true|true||2,4,6");
}

#[test]
fn url_statics_parse_and_can_parse() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        var out = "";
        out += URL.canParse("https://ok.example/") + "|";
        out += URL.canParse("not a url") + "|";
        out += URL.canParse("/rel", "https://base.example") + "|";
        const parsed = URL.parse("https://p.example/x");
        out += (parsed instanceof URL) + "|" + parsed.pathname + "|";
        out += (URL.parse("nope") === null) + "|";
        out += URL.parse.length + "," + URL.canParse.length;
        out
        "#,
    );
    assert_eq!(result, "true|false|true|true|/x|true|1,1");
}

#[test]
fn headers_normalize_combine_and_iterate() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        const headers = new Headers({ "X-B": "2", "X-A": "1" });
        headers.append("Content-Type", " text/plain ");
        headers.append("content-type", "charset=utf-8");
        const combined = headers.get("CONTENT-TYPE");
        const ordered = [...headers].map(([k, v]) => k + "=" + v).join("|");
        headers.set("x-a", "one");
        headers.delete("x-b");
        combined + "%" + ordered + "%" + headers.get("x-a") + "%" + headers.has("x-b")
        "#,
    );
    assert_eq!(
        result,
        "text/plain, charset=utf-8%content-type=text/plain, charset=utf-8|x-a=1|x-b=2%one%false"
    );
}

#[test]
fn request_parses_standard_init_and_reads_body() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        var out = "";
        const request = new Request("https://example.com/api?x=1", {
          method: "post",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ ping: "pong" }),
        });
        out += request.method + "|" + request.url + "|";
        out += request.headers.get("content-type") + "|";
        out += (request instanceof Request) + "|";
        out += (Object.getPrototypeOf(request) === Request.prototype) + "|";
        const cloned = request.clone();
        request.json().then((data) => { out += data.ping + "|" + request.bodyUsed; });
        out
        "#,
    );
    assert_eq!(
        result,
        "POST|https://example.com/api?x=1|application/json|true|true|"
    );
    let after = eval_string(&mut runtime, "out");
    assert_eq!(
        after,
        "POST|https://example.com/api?x=1|application/json|true|true|pong|true"
    );
}

#[test]
fn request_text_awaits_inside_async_function() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        var out = "pending";
        const request = new Request("https://example.com/api", {
          method: "post",
          body: "hello",
        });
        async function readBody() {
          return await request.text();
        }
        readBody().then(
          (text) => { out = text; },
          (err) => { out = "ERR:" + err; },
        );
        out
        "#,
    );
    assert_eq!(result, "pending");
    let after = eval_string(&mut runtime, "out");
    assert_eq!(after, "hello");
}

#[test]
fn request_rejects_bodied_get_and_forbidden_methods() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        var out = "";
        try { new Request("https://x/", { body: "nope" }); } catch (e) { out += e.constructor.name; }
        try { new Request("https://x/", { method: "TRACE" }); } catch (e) { out += "|" + e.constructor.name; }
        out
        "#,
    );
    assert_eq!(result, "TypeError|TypeError");
}

#[test]
fn response_honors_init_statics_and_body_mixin() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        var out = "";
        const response = new Response("created", {
          status: 201,
          statusText: "Created",
          headers: [["X-Trace", "abc"]],
        });
        out += response.status + "|" + response.statusText + "|" + response.ok + "|";
        out += response.headers.get("x-trace") + "|";
        out += (response instanceof Response) + "|";
        out += response.headers.get("content-type") + "|";
        const json = Response.json({ n: 7 }, { status: 202 });
        out += json.status + "|" + json.headers.get("content-type") + "|";
        const redirect = Response.redirect("https://example.com/next", 307);
        out += redirect.status + "|" + redirect.headers.get("location") + "|";
        out += Response.error().type;
        response.text().then((text) => { out += "|" + text; });
        out
        "#,
    );
    assert_eq!(
        result,
        "201|Created|true|abc|true|text/plain;charset=UTF-8|202|application/json|307|https://example.com/next|error"
    );
    let after = eval_string(&mut runtime, "out");
    assert!(after.ends_with("|created"), "unexpected: {after}");
}

#[test]
fn response_body_supports_bytes_stream_and_clone() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        var out = "";
        const bytes = new TextEncoder().encode("binary");
        const fromBytes = new Response(bytes);
        const cloned = fromBytes.clone();
        fromBytes.arrayBuffer().then((buffer) => {
          out += new TextDecoder().decode(new Uint8Array(buffer));
          return cloned.text();
        }).then((text) => { out += "|" + text; });
        const stream = new ReadableStream({
          start(controller) {
            controller.enqueue(new TextEncoder().encode("st"));
            controller.enqueue(new TextEncoder().encode("ream"));
            controller.close();
          },
        });
        new Response(stream).text().then((text) => { out += "|" + text; });
        out
        "#,
    );
    assert_eq!(result, "");
    let after = eval_string(&mut runtime, "out");
    assert_eq!(after, "binary|binary|stream");
}

#[test]
fn request_form_data_parses_urlencoded_and_multipart() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    eval_string(
        &mut runtime,
        r#"
        var out = "";
        new Request("https://x/", {
          method: "POST",
          headers: { "content-type": "application/x-www-form-urlencoded" },
          body: "a=1&b=two+words",
        }).formData().then((form) => { out += form.get("a") + "|" + form.get("b"); });
        const form = new FormData();
        form.append("name", "otter");
        new Request("https://x/", { method: "POST", body: form })
          .formData().then((parsed) => { out += "|" + parsed.get("name"); });
        "#,
    );
    let after = eval_string(&mut runtime, "out");
    assert_eq!(after, "1|two words|otter");
}

#[test]
fn fetch_internals_round_trip_for_server_glue() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        void Request; // touch a lazy fetch global so the internals install
        const internals = __otterFetchInternals;
        const request = internals.makeRequest(
          "POST", "http://h/echo", ["content-type", "application/json"], '{"n":1}');
        let out = request.method + "|" + request.url + "|" +
          request.headers.get("content-type");
        request.json().then((data) => { out += "|" + data.n; });
        const parts = internals.responseParts(new Response("ok", {
          status: 201,
          headers: { "x-a": "1" },
        }));
        out += "%" + parts[0] + "%" + parts[2].join(",") + "%" + parts[3];
        out
        "#,
    );
    assert_eq!(
        result,
        "POST|http://h/echo|application/json%201%content-type,text/plain;charset=UTF-8,x-a,1%ok"
    );
    // `request.json()` is a real promise, so its `.then` callback runs as a
    // microtask after the synchronous script (which appends the `%` response
    // parts) — the `|1` lands last, matching engine-independent spec ordering.
    let after = eval_string(&mut runtime, "out");
    assert_eq!(
        after,
        "POST|http://h/echo|application/json%201%content-type,text/plain;charset=UTF-8,x-a,1%ok|1"
    );
}

#[test]
fn web_api_globals_install_and_run_through_runtime_builder() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        const url = new URL("https://example.com/a?x=1");
        const headers = new Headers();
        headers.append("Content-Type", " text/plain ");
        const blob = new Blob(["hello"], { type: "TEXT/PLAIN" });
        const request = new Request("https://example.com/api", { method: "post" });
        const response = Response.json({ ok: true });
        url.href + "|" + headers.get("content-type") + "|" + blob.size + ":" + blob.type + "|" +
          request.method + "|" + response.status
        "#,
    );
    assert_eq!(
        result,
        "https://example.com/a?x=1|text/plain|5:text/plain|POST|200"
    );
}

/// Real end-to-end WebAssembly: validate a module, instantiate it with an
/// imported JS function, call an exported function that both does integer
/// math and re-enters the import, and read the exported linear memory.
#[test]
fn web_assembly_validates_instantiates_and_runs() {
    // A module that imports `env.addThree`, exports `add` (i32.add),
    // exports `callImport` (forwards to the JS import), and exports a
    // 1-page `memory` whose byte 0 is initialized to 42.
    let wasm = wat::parse_str(
        r#"
        (module
          (import "env" "addThree" (func $addThree (param i32) (result i32)))
          (memory (export "memory") 1)
          (data (i32.const 0) "\2a")
          (func (export "add") (param i32 i32) (result i32)
            local.get 0
            local.get 1
            i32.add)
          (func (export "callImport") (param i32) (result i32)
            local.get 0
            call $addThree))
        "#,
    )
    .expect("wat compiles to wasm bytes");

    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    // Hand the wasm bytes to JS as a base64 string the harness decodes.
    let base64 = {
        use std::fmt::Write as _;
        let mut encoded = String::new();
        for byte in &wasm {
            let _ = write!(encoded, "{byte},");
        }
        encoded
    };

    let result = eval_string(
        &mut runtime,
        &format!(
            r#"
        globalThis.out = "pending";
        const bytes = Uint8Array.from("{base64}".split(",").filter((s) => s.length).map(Number));
        // Synchronous validation of real wasm bytes.
        const valid = WebAssembly.validate(bytes);
        const invalid = WebAssembly.validate(new Uint8Array([0, 1, 2, 3]));
        const importObject = {{ env: {{ addThree: (n) => n + 3 }} }};
        WebAssembly.instantiate(bytes, importObject).then((result) => {{
          const instance = result.instance;
          const exports = instance.exports;
          const sum = exports.add(20, 22);                 // pure i32 math
          const forwarded = exports.callImport(39);        // re-enters the JS import
          const memoryOk = exports.memory instanceof WebAssembly.Memory;
          const firstByte = new Uint8Array(exports.memory.buffer)[0]; // read exported memory
          const grown = exports.memory.grow(1);
          globalThis.out = [
            valid, invalid,
            result.module instanceof WebAssembly.Module,
            instance instanceof WebAssembly.Instance,
            sum, forwarded, memoryOk, firstByte, grown,
            new Uint8Array(exports.memory.buffer).length,
          ].join("|");
        }}, (err) => {{ globalThis.out = "ERR:" + (err && err.stack || err); }});
        "pending"
        "#,
        ),
    );
    assert_eq!(result, "pending");
    let after = eval_string(&mut runtime, "out");
    // valid | invalid | module-branded | instance-branded | 20+22 | 39+3 |
    // memory-branded | data byte | grow-returns-old-pages | new size (2 pages).
    assert_eq!(after, "true|false|true|true|42|42|true|42|1|131072");
}

/// The synchronous `new WebAssembly.Module` / `new WebAssembly.Instance`
/// constructors, `Global`, and the typed error classes.
#[test]
fn web_assembly_constructors_globals_and_errors() {
    let wasm = wat::parse_str(
        r#"
        (module
          (func (export "id") (param i32) (result i32) local.get 0))
        "#,
    )
    .expect("wat compiles");
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let base64 = {
        use std::fmt::Write as _;
        let mut encoded = String::new();
        for byte in &wasm {
            let _ = write!(encoded, "{byte},");
        }
        encoded
    };
    let result = eval_string(
        &mut runtime,
        &format!(
            r#"
        var out = "";
        const bytes = Uint8Array.from("{base64}".split(",").filter((s) => s.length).map(Number));
        const module = new WebAssembly.Module(bytes);
        out += (module instanceof WebAssembly.Module) + "|";
        const instance = new WebAssembly.Instance(module);
        out += (instance instanceof WebAssembly.Instance) + "|";
        out += instance.exports.id(7) + "|";
        // A mutable Global round-trips its value.
        const global = new WebAssembly.Global({{ value: "i32", mutable: true }}, 5);
        out += global.value + "|";
        global.value = 11;
        out += global.value + "|";
        // The error classes are Error subclasses on the namespace.
        out += (new WebAssembly.CompileError("x") instanceof Error) + "|";
        out += (WebAssembly.LinkError.name) + "|";
        // A bad instantiate rejects with a LinkError (missing import).
        globalThis.linkOut = "pending";
        WebAssembly.instantiate(
          Uint8Array.from(bytes),
          undefined,
        ).then(() => {{ globalThis.linkOut = "resolved"; }});
        // Instantiating a module that needs an import without one throws/rejects;
        // here `id` needs none, so this resolves — assert the class name instead.
        out += WebAssembly.RuntimeError.name;
        out
        "#,
        ),
    );
    assert_eq!(result, "true|true|7|5|11|true|LinkError|RuntimeError");
}

/// wasmtime-backed capabilities beyond the wasmi baseline: cross-store imports
/// (a standalone `Memory` linked into an instance), `i64` <-> `BigInt`, and an
/// `externref` global round-tripping a JS object by identity.
#[test]
fn web_assembly_cross_store_bigint_and_externref() {
    // Imports a memory + an i64 helper, exports a func that writes 42 into the
    // imported memory, an i64 adder, and a mutable externref global.
    let wasm = wat::parse_str(
        r#"
        (module
          (import "js" "mem" (memory 1))
          (func (export "poke")
            i32.const 0
            i32.const 42
            i32.store)
          (func (export "add64") (param i64 i64) (result i64)
            local.get 0
            local.get 1
            i64.add)
          (global (export "slot") (mut externref) (ref.null extern)))
        "#,
    )
    .expect("wat compiles");
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let base64 = {
        use std::fmt::Write as _;
        let mut encoded = String::new();
        for byte in &wasm {
            let _ = write!(encoded, "{byte},");
        }
        encoded
    };
    let result = eval_string(
        &mut runtime,
        &format!(
            r#"
        globalThis.out = "pending";
        const bytes = Uint8Array.from("{base64}".split(",").filter((s) => s.length).map(Number));
        // A standalone Memory, then imported into the instance (cross-store).
        const mem = new WebAssembly.Memory({{ initial: 1 }});
        WebAssembly.instantiate(bytes, {{ js: {{ mem }} }}).then((result) => {{
          const ex = result.instance.exports;
          ex.poke();
          // The write is visible through the SAME standalone Memory object.
          const crossStore = new Uint8Array(mem.buffer)[0];
          // i64 params/results are BigInt.
          const sum = ex.add64(9007199254740993n, 1n); // > 2^53, exact only as BigInt
          const isBig = typeof sum === "bigint";
          // externref global round-trips a JS object by identity.
          const sentinel = {{ tag: "otter" }};
          ex.slot.value = sentinel;
          const sameRef = ex.slot.value === sentinel;
          globalThis.out = [crossStore, sum.toString(), isBig, sameRef].join("|");
        }}, (err) => {{ globalThis.out = "ERR:" + (err && err.stack || err); }});
        "pending"
        "#,
        ),
    );
    assert_eq!(result, "pending");
    let after = eval_string(&mut runtime, "out");
    // written-byte | 2^53+1 + 1 = 9007199254740994 | bigint | identity preserved.
    assert_eq!(after, "42|9007199254740994|true|true");
}

/// Encode wasm bytes as the comma-separated byte string the test harness
/// decodes back into a `Uint8Array`.
fn wasm_byte_literal(wasm: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::new();
    for byte in wasm {
        let _ = write!(encoded, "{byte},");
    }
    encoded
}

/// A wasm module that defines an exception tag, exports it, and exports a
/// function that `throw`s it with an `i32` payload. JS catches the escaping
/// throw as a `WebAssembly.Exception`, confirms its tag identity, and reads the
/// payload back.
#[test]
fn web_assembly_tagged_exception_crosses_to_js() {
    let wasm = wat::parse_str(
        r#"
        (module
          (tag $e (export "e") (param i32))
          (func (export "boom") (param i32)
            local.get 0
            throw $e))
        "#,
    )
    .expect("wat compiles");
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let base64 = wasm_byte_literal(&wasm);
    let result = eval_string(
        &mut runtime,
        &format!(
            r#"
        var out = "";
        const bytes = Uint8Array.from("{base64}".split(",").filter((s) => s.length).map(Number));
        const instance = new WebAssembly.Instance(new WebAssembly.Module(bytes));
        const tag = instance.exports.e;
        out += (tag instanceof WebAssembly.Tag) + "|";
        try {{
          instance.exports.boom(99);
          out += "NO-THROW";
        }} catch (e) {{
          out += (e instanceof WebAssembly.Exception) + "|";
          out += e.is(tag) + "|";
          out += e.getArg(tag, 0);
        }}
        out
        "#,
        ),
    );
    // tag-branded | exception-branded | tag identity | payload read back.
    assert_eq!(result, "true|true|true|99");
}

/// A JS-constructed `WebAssembly.Exception` round-trips its tag identity and
/// payload, and `WebAssembly.JSTag` is a well-known `WebAssembly.Tag`.
#[test]
fn web_assembly_exception_constructed_in_js_and_jstag() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        var out = "";
        const tag = new WebAssembly.Tag({ parameters: ["i32"] });
        const exn = new WebAssembly.Exception(tag, [42]);
        out += (exn instanceof WebAssembly.Exception) + "|";
        out += exn.is(tag) + "|";
        out += exn.getArg(tag, 0) + "|";
        // A different tag is not this exception's tag.
        const other = new WebAssembly.Tag({ parameters: ["i32"] });
        out += exn.is(other) + "|";
        // JSTag is a well-known Tag instance.
        out += (WebAssembly.JSTag instanceof WebAssembly.Tag);
        out
        "#,
    );
    // exception-branded | tag identity | payload | wrong-tag false | JSTag is a Tag.
    assert_eq!(result, "true|true|42|false|true");
}

/// A JS import that throws crosses wasm frames via the `JSTag`: an uncaught
/// throw propagates back through the export call and surfaces to JS as the
/// original thrown value, with identity preserved.
#[test]
fn web_assembly_js_import_throw_crosses_wasm_frames() {
    let wasm = wat::parse_str(
        r#"
        (module
          (import "js" "boom" (func $boom))
          (func (export "call") (call $boom)))
        "#,
    )
    .expect("wat compiles");
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let base64 = wasm_byte_literal(&wasm);
    let result = eval_string(
        &mut runtime,
        &format!(
            r#"
        var out = "";
        const bytes = Uint8Array.from("{base64}".split(",").filter((s) => s.length).map(Number));
        const sentinel = new Error("from-js");
        const instance = new WebAssembly.Instance(new WebAssembly.Module(bytes), {{
          js: {{ boom: () => {{ throw sentinel; }} }},
        }});
        try {{
          instance.exports.call();
          out += "NO-THROW";
        }} catch (e) {{
          // The original JS error object crosses the wasm frame with identity.
          out += (e === sentinel) + "|" + (e.message === "from-js");
        }}
        out
        "#,
        ),
    );
    assert_eq!(result, "true|true");
}

/// The authoritative WinterTC / ECMA-429 Minimum Common API ledger.
///
/// Every path named by the ECMA-429 minimum-common-API snapshot appears in
/// exactly one bucket:
///
/// - `SUPPORTED` — present on `globalThis` and spec-shaped for the common case.
/// - `PARTIAL`   — present, but with a known, documented behavior gap. Each
///   `PARTIAL` entry must also carry an executable smoke check in
///   [`wintertc_partial_entries_document_their_gap`].
/// - `NOT_YET`   — not yet installed; must be absent (`undefined`).
///
/// Shared with the smoke test so a `PARTIAL` entry can never drift away from
/// its documenting check.
const WINTERTC_LEDGER_JS: &str = r#"
    const SUPPORTED = [
      "globalThis",
      "self",
      "reportError",
      "atob",
      "btoa",
      "queueMicrotask",
      "setTimeout",
      "clearTimeout",
      "setInterval",
      "clearInterval",
      "console",
      "crypto",
      "Crypto",
      "CryptoKey",
      "SubtleCrypto",
      "crypto.getRandomValues",
      "crypto.randomUUID",
      "performance",
      "Performance",
      "performance.now",
      "performance.timeOrigin",
      "navigator",
      "Navigator",
      "navigator.userAgent",
      "AbortController",
      "AbortSignal",
      "Blob",
      "File",
      "FormData",
      "ByteLengthQueuingStrategy",
      "CountQueuingStrategy",
      "CompressionStream",
      "DecompressionStream",
      "CustomEvent",
      "ErrorEvent",
      "Event",
      "EventTarget",
      "MessageEvent",
      "DOMException",
      "Headers",
      "Request",
      "Response",
      "MessageChannel",
      "MessagePort",
      "ReadableStream",
      "ReadableByteStreamController",
      "ReadableStreamBYOBReader",
      "ReadableStreamBYOBRequest",
      "ReadableStreamDefaultController",
      "ReadableStreamDefaultReader",
      "WritableStream",
      "WritableStreamDefaultController",
      "WritableStreamDefaultWriter",
      "TransformStream",
      "TextDecoder",
      "TextDecoderStream",
      "TextEncoder",
      "TextEncoderStream",
      "TransformStreamDefaultController",
      "PromiseRejectionEvent",
      "URLSearchParams",
      "URL",
      "URLPattern",
      "fetch",
      "structuredClone",
      "crypto.subtle",
      "crypto.subtle.digest",
      "onerror",
      "onunhandledrejection",
      "onrejectionhandled",
      "WebAssembly",
      "WebAssembly.compile",
      "WebAssembly.compileStreaming",
      "WebAssembly.instantiate",
      "WebAssembly.instantiateStreaming",
      "WebAssembly.validate",
      "WebAssembly.Module",
      "WebAssembly.Instance",
      "WebAssembly.Memory",
      "WebAssembly.Table",
      "WebAssembly.Global",
      "WebAssembly.CompileError",
      "WebAssembly.LinkError",
      "WebAssembly.RuntimeError",
      "WebAssembly.Tag",
      "WebAssembly.Exception",
      "WebAssembly.JSTag",
    ];
    const PARTIAL = [];
    // Otter's global object is not a Window/Worker EventTarget, so the global
    // event-handler IDL attributes are exposed as plain settable
    // ([Replaceable]-equivalent) globals rather than through addEventListener.
    // `onunhandledrejection` / `onrejectionhandled` are invoked by the VM's
    // HostPromiseRejectionTracker checkpoint; `onerror` is present for parity
    // (uncaught exceptions still surface as runtime diagnostics).
    const OMITTED = [];
    const NOT_YET = [];
    function lookup(path) {
      let value = globalThis;
      for (const part of path.split(".")) {
        if (value === undefined || value === null) return undefined;
        value = value[part];
      }
      return value;
    }
"#;

/// Each ECMA-429 path is classified exactly once, and the runtime surface
/// matches its bucket: `SUPPORTED`/`PARTIAL` are present, `NOT_YET` is absent.
///
/// Implementing a `NOT_YET` API makes this fail with a "move" message; promote
/// the name to `PARTIAL` (with a smoke check) or `SUPPORTED` in the same change
/// so the ledger always mirrors reality.
#[test]
fn wintertc_minimum_common_api_ledger() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        &format!(
            "{WINTERTC_LEDGER_JS}\n{}",
            r#"
        const problems = [];
        const seen = new Map();
        for (const [bucket, list] of [["SUPPORTED", SUPPORTED], ["PARTIAL", PARTIAL], ["NOT_YET", NOT_YET], ["OMITTED", OMITTED]]) {
          for (const name of list) {
            if (seen.has(name)) problems.push("duplicate ledger entry: " + name + " in " + seen.get(name) + " and " + bucket);
            seen.set(name, bucket);
          }
        }
        for (const name of SUPPORTED) {
          if (lookup(name) === undefined) problems.push("missing SUPPORTED API: " + name);
        }
        for (const name of PARTIAL) {
          if (lookup(name) === undefined) problems.push("missing PARTIAL API: " + name);
        }
        for (const name of NOT_YET) {
          if (lookup(name) !== undefined) {
            problems.push("implemented but listed NOT_YET (promote it): " + name);
          }
        }
        for (const name of OMITTED) {
          if (lookup(name) !== undefined) {
            problems.push("OMITTED-by-design API is present: " + name);
          }
        }
        problems.join("; ")
        "#,
        ),
    );
    assert_eq!(result, "");
}

/// Every `PARTIAL` ledger entry carries an executable check that still observes
/// its documented limitation. When a gap is closed, its check flips and this
/// test fails, forcing the entry to be promoted to `SUPPORTED`.
#[test]
fn wintertc_partial_entries_document_their_gap() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        &format!(
            "{WINTERTC_LEDGER_JS}\n{}",
            r#"
        // Each smoke check returns true while the documented gap is still open.
        // Empty: every WinterTC path is now SUPPORTED, OMITTED, or NOT_YET.
        const GAPS = {};
        const problems = [];
        for (const name of PARTIAL) {
          const check = GAPS[name];
          if (!check) { problems.push("PARTIAL entry without smoke check: " + name); continue; }
          let held;
          try { held = check(); } catch (e) { problems.push("smoke check threw for " + name + ": " + e.message); continue; }
          if (!held) problems.push("documented gap closed, promote to SUPPORTED: " + name);
        }
        for (const name of Object.keys(GAPS)) {
          if (!PARTIAL.includes(name)) problems.push("smoke check for non-PARTIAL entry: " + name);
        }
        problems.join("; ")
        "#,
        ),
    );
    assert_eq!(result, "");
}

#[test]
fn global_scope_shell_self_and_report_error() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        var out = "";
        out += (self === globalThis) + "|";
        // `self` is a replaceable accessor: assigning shadows it with a data property.
        const desc = Object.getOwnPropertyDescriptor(globalThis, "self");
        out += (typeof desc.get === "function") + "|" + desc.enumerable + "|";
        // `reportError` is a callable, non-enumerable global that does not throw.
        out += typeof reportError + "|";
        out += Object.getOwnPropertyDescriptor(globalThis, "reportError").enumerable + "|";
        let threw = false;
        try { reportError(new TypeError("boom")); } catch (e) { threw = true; }
        out += threw;
        out
        "#,
    );
    assert_eq!(result, "true|true|true|function|false|false");
}

#[test]
fn unhandled_rejection_notifies_and_rejectionhandled_follows() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    // First turn: install handlers and reject promises. The
    // unhandledrejection/rejectionhandled events fire during the microtask drain
    // the runtime performs after the synchronous script completes, so the log is
    // only populated by the time control returns here.
    eval_string(
        &mut runtime,
        r#"
        globalThis.__log = [];
        const log = globalThis.__log;
        globalThis.onunhandledrejection = (e) => {
          log.push("U:" + e.reason + ":" + (e.promise instanceof Promise));
          e.preventDefault(); // suppress the default console report
          if (e.reason === "late") {
            // Attach a handler after the promise was reported unhandled so the
            // follow-up rejectionhandled notification fires.
            queueMicrotask(() => { e.promise.catch(() => {}); });
          }
        };
        globalThis.onrejectionhandled = (e) => { log.push("H:" + e.reason); };

        Promise.reject("boom");                 // reported unhandled, stays so
        Promise.reject("late");                 // reported unhandled, then handled
        Promise.reject("quiet").catch(() => {}); // handled synchronously: silent
        undefined
        "#,
    );
    // Second turn: read the log the drained checkpoint populated.
    let result = eval_string(&mut runtime, "globalThis.__log.join(',')");
    let entries: Vec<&str> = result.split(',').filter(|s| !s.is_empty()).collect();
    assert!(
        entries.contains(&"U:boom:true"),
        "expected unhandledrejection for boom, got {result:?}"
    );
    assert!(
        entries.contains(&"U:late:true"),
        "expected unhandledrejection for late, got {result:?}"
    );
    assert!(
        entries.contains(&"H:late"),
        "expected rejectionhandled for late, got {result:?}"
    );
    assert!(
        !entries.iter().any(|e| e.contains("quiet")),
        "a synchronously-handled rejection must not notify, got {result:?}"
    );
    // rejectionhandled must come after the promise was reported unhandled.
    let u_late = entries.iter().position(|e| *e == "U:late:true");
    let h_late = entries.iter().position(|e| *e == "H:late");
    assert!(
        u_late < h_late,
        "rejectionhandled before report, got {result:?}"
    );
}

#[test]
fn navigator_reports_otter_user_agent() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        navigator.userAgent + "|" + (navigator.userAgentData === undefined) + "|" +
          Object.getOwnPropertyDescriptor(globalThis, "navigator").enumerable
        "#,
    );
    assert_eq!(
        result,
        format!("Otter/{}|true|false", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn queuing_strategies_follow_spec_shape() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        var out = "";
        const bls = new ByteLengthQueuingStrategy({ highWaterMark: 16 });
        out += bls.highWaterMark + "|" + bls.size(new Uint8Array(8)) + "|";
        out += (bls.size === new ByteLengthQueuingStrategy({ highWaterMark: 1 }).size) + "|";
        const cqs = new CountQueuingStrategy({ highWaterMark: 4 });
        out += cqs.highWaterMark + "|" + cqs.size("anything") + "|";
        out += bls.size.name + "," + bls.size.length + "|" + cqs.size.name + "," + cqs.size.length + "|";
        try { new CountQueuingStrategy(); } catch (e) { out += e.constructor.name + "|"; }
        try { new ByteLengthQueuingStrategy({}); } catch (e) { out += e.constructor.name + "|"; }
        const rs = new ReadableStream({}, new CountQueuingStrategy({ highWaterMark: 3 }));
        out += (rs instanceof ReadableStream);
        out
        "#,
    );
    assert_eq!(
        result,
        "16|8|true|4|1|size,1|size,0|TypeError|TypeError|true"
    );
}

#[test]
fn queue_microtask_runs_indirect_callers() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        var out = "";
        const indirect = queueMicrotask;
        try { indirect(null); } catch (e) { out += e.constructor.name; }
        indirect(() => { out += "|ran"; });
        out
        "#,
    );
    assert_eq!(result, "TypeError");
    // The queued callback drains at the eval checkpoint; observe it after.
    let after = eval_string(&mut runtime, "out");
    assert_eq!(after, "TypeError|ran");
}

#[test]
fn structured_clone_transfer_detaches_array_buffer() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = runtime
        .eval(SourceInput::from_javascript(
            r#"
            const buffer = new ArrayBuffer(4);
            const view = new Uint8Array(buffer);
            view[0] = 7;
            const clone = structuredClone(buffer, { transfer: [buffer] });
            if (clone.byteLength !== 4) throw new Error("clone length");
            if (new Uint8Array(clone)[0] !== 7) throw new Error("clone bytes");
            if (buffer.byteLength !== 0) throw new Error("source not detached");
            let detached = false;
            try {
              new Uint8Array(buffer);
            } catch {
              detached = true;
            }
            if (!detached) throw new Error("view on detached buffer");
            "ok";
            "#,
        ))
        .unwrap();
    assert_eq!(result.completion_string(), "ok");
}

#[test]
fn crypto_get_random_values_fills_and_returns_same_array() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        const array = new Uint8Array(32);
        const returned = crypto.getRandomValues(array);
        // 32 zero bytes from a CSPRNG has probability 2^-256.
        const filled = array.some((byte) => byte !== 0);
        const offsetView = new Uint32Array(new ArrayBuffer(16), 4, 2);
        crypto.getRandomValues(offsetView);
        (returned === array) + "|" + filled + "|" + array.length
        "#,
    );
    assert_eq!(result, "true|true|32");
}

#[test]
fn crypto_get_random_values_rejects_non_integer_and_oversized_arrays() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        var out = "";
        try { crypto.getRandomValues(new Float64Array(4)); }
        catch (e) { out += e.name + "," + (e instanceof DOMException); }
        try { crypto.getRandomValues(new DataView(new ArrayBuffer(4))); }
        catch (e) { out += "|" + e.name; }
        try { crypto.getRandomValues(new Uint8Array(65537)); }
        catch (e) { out += "|" + e.name; }
        out += "|" + crypto.getRandomValues(new Uint8Array(65536)).length;
        out
        "#,
    );
    assert_eq!(
        result,
        "TypeMismatchError,true|TypeMismatchError|QuotaExceededError|65536"
    );
}

#[test]
fn crypto_random_uuid_is_version_4_and_unique() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        const first = crypto.randomUUID();
        const second = crypto.randomUUID();
        const shape = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
        shape.test(first) + "|" + shape.test(second) + "|" + (first !== second)
        "#,
    );
    assert_eq!(result, "true|true|true");
}

#[test]
fn crypto_subtle_digest_matches_known_vectors_and_rejects_unknowns() {
    let mut runtime = Runtime::builder().with_web_apis().build().unwrap();
    let result = eval_string(
        &mut runtime,
        r#"
        // Slot-indexed results: reaction ORDER between independent
        // promises is not part of the contract (a真 async digest adds
        // an adoption tick), only the values are.
        var slots = ["", "", "", ""];
        globalThis.slots = slots;
        function hex(buffer) {
          return Array.from(new Uint8Array(buffer))
            .map((byte) => ("0" + byte.toString(16)).slice(-2))
            .join("");
        }
        slots[0] = String(crypto.subtle === crypto.subtle);
        crypto.subtle.digest("SHA-256", new TextEncoder().encode("abc"))
          .then((buffer) => { slots[1] = hex(buffer); });
        crypto.subtle.digest({ name: "sha-1" }, new ArrayBuffer(0))
          .then((buffer) => { slots[2] = hex(buffer); });
        crypto.subtle.digest("MD5", new Uint8Array(0))
          .catch((error) => { slots[3] = error.name; });
        slots[0]
        "#,
    );
    assert_eq!(result, "true");
    let after = eval_string(&mut runtime, "slots.join('|')");
    assert_eq!(
        after,
        "true|ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad|\
         da39a3ee5e6b4b0d3255bfef95601890afd80709|NotSupportedError"
    );
}
