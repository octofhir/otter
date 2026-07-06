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
        ["URL", "Blob"]
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
        "201|Created|false|abc|true|text/plain;charset=UTF-8|202|application/json|307|https://example.com/next|error"
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
    let after = eval_string(&mut runtime, "out");
    assert!(after.starts_with("POST|http://h/echo|application/json|1%"));
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
        const blob = new Blob("hello", "TEXT/PLAIN");
        const request = new Request("https://example.com/api", { method: "post" });
        const response = Response.json({ ok: true });
        url.href + "|" + headers.get("content-type") + "|" + blob.text() + "|" +
          request.method + "|" + response.status
        "#,
    );
    assert_eq!(
        result,
        "https://example.com/a?x=1|text/plain|hello|POST|200"
    );
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
