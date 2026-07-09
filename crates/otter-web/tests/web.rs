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
        ["URL", "Blob", "File"]
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
      "crypto.getRandomValues",
      "crypto.randomUUID",
      "performance",
      "performance.now",
      "performance.timeOrigin",
      "navigator",
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
      "MessageChannel",
      "MessagePort",
      "ReadableStream",
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
      "URLSearchParams",
    ];
    const PARTIAL = [
      "fetch",
      "URL",
      "Request",
      "Response",
      "crypto.subtle",
      "crypto.subtle.digest",
      "structuredClone",
    ];
    const NOT_YET = [
      "onerror",
      "onunhandledrejection",
      "onrejectionhandled",
      "Crypto",
      "CryptoKey",
      "SubtleCrypto",
      "Navigator",
      "Performance",
      "PromiseRejectionEvent",
      "ReadableByteStreamController",
      "ReadableStreamBYOBReader",
      "ReadableStreamBYOBRequest",
      "TransformStreamDefaultController",
      "URLPattern",
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
      "WebAssembly.Tag",
      "WebAssembly.Exception",
      "WebAssembly.CompileError",
      "WebAssembly.LinkError",
      "WebAssembly.RuntimeError",
      "WebAssembly.JSTag",
    ];
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
        for (const [bucket, list] of [["SUPPORTED", SUPPORTED], ["PARTIAL", PARTIAL], ["NOT_YET", NOT_YET]]) {
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
        const GAPS = {
          // fetch() is a placeholder that throws instead of performing a request.
          "fetch": () => {
            try { fetch("http://example.invalid"); return false; }
            catch (e) { return String(e.message).includes("fetch is not implemented"); }
          },
          // URL exposes snapshot data properties, not live spec accessors.
          "URL": () => {
            const u = new URL("https://e.com/p");
            const d = Object.getOwnPropertyDescriptor(u, "href")
              || Object.getOwnPropertyDescriptor(Object.getPrototypeOf(u), "href");
            return !!(d && "value" in d) && !(d && d.get);
          },
          // Request bodies are default streams only; no BYOB byte reader yet.
          "Request": () => {
            const b = new Request("https://e.com", { method: "POST", body: "hi" }).body;
            try { b.getReader({ mode: "byob" }); return false; }
            catch (e) { return String(e.message).includes("byob"); }
          },
          // Response bodies are likewise default streams with no BYOB reader.
          "Response": () => {
            const b = new Response("x").body;
            try { b.getReader({ mode: "byob" }); return false; }
            catch (e) { return String(e.message).includes("byob"); }
          },
          // Only digest is wired on SubtleCrypto; keys/sign/encrypt are absent.
          "crypto.subtle": () => typeof crypto.subtle.encrypt === "undefined"
            && typeof crypto.subtle.importKey === "undefined",
          "crypto.subtle.digest": () => typeof crypto.subtle.digest === "function"
            && typeof crypto.subtle.sign === "undefined",
          // structuredClone cannot transfer transferables such as MessagePort.
          "structuredClone": () => {
            const mc = new MessageChannel();
            try { structuredClone({ p: mc.port1 }, { transfer: [mc.port1] }); return false; }
            catch (e) { return true; }
          },
        };
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
        var out = "";
        function hex(buffer) {
          return Array.from(new Uint8Array(buffer))
            .map((byte) => ("0" + byte.toString(16)).slice(-2))
            .join("");
        }
        out += (crypto.subtle === crypto.subtle) + "|";
        crypto.subtle.digest("SHA-256", new TextEncoder().encode("abc"))
          .then((buffer) => { out += hex(buffer); });
        crypto.subtle.digest({ name: "sha-1" }, new ArrayBuffer(0))
          .then((buffer) => { out += "|" + hex(buffer); });
        crypto.subtle.digest("MD5", new Uint8Array(0))
          .catch((error) => { out += "|" + error.name; });
        out
        "#,
    );
    assert_eq!(result, "true|");
    let after = eval_string(&mut runtime, "out");
    assert_eq!(
        after,
        "true|ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad|\
         da39a3ee5e6b4b0d3255bfef95601890afd80709|NotSupportedError"
    );
}
