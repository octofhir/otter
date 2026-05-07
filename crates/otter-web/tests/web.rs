use otter_runtime::{Runtime, SourceInput};
use otter_web::blob::Blob;
use otter_web::headers::Headers;
use otter_web::request_response::{Request, Response};
use otter_web::url::WebUrl;
use otter_web::web_api_classes;

#[test]
fn web_api_specs_are_static_and_ordered() {
    let specs = web_api_classes();
    assert_eq!(
        specs.iter().map(|spec| spec.name).collect::<Vec<_>>(),
        ["URL", "Headers", "Blob", "Request", "Response"]
    );
    assert_eq!(specs[0].spec.name(), "URL");
    assert_eq!(specs[2].spec.name(), "Blob");
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
fn headers_normalize_and_combine_values() {
    let mut headers = Headers::new();
    headers.append("Content-Type", " text/plain ").unwrap();
    headers.append("content-type", "charset=utf-8").unwrap();
    assert_eq!(
        headers.get("CONTENT-TYPE").unwrap(),
        Some("text/plain, charset=utf-8".to_string())
    );
    assert!(headers.has("content-type").unwrap());
    assert_eq!(
        headers.entries(),
        vec![(
            "content-type".to_string(),
            "text/plain, charset=utf-8".to_string()
        )]
    );
}

#[test]
fn blob_slices_and_decodes_text() {
    let blob = Blob::new(b"hello world".to_vec(), "TEXT/PLAIN");
    assert_eq!(blob.size(), 11);
    assert_eq!(blob.content_type(), "text/plain");
    assert_eq!(blob.slice(6, None, None).text(), "world");
}

#[test]
fn request_response_hold_owned_fetch_records() {
    let request = Request::new("https://example.com/data", Some("post"), None).unwrap();
    assert_eq!(request.method(), "POST");
    assert_eq!(request.url(), "https://example.com/data");
    let response = Response::new(
        201,
        "Created",
        Some(Blob::new(b"ok".to_vec(), "text/plain")),
    )
    .unwrap();
    assert_eq!(response.status(), 201);
    assert_eq!(response.status_text(), "Created");
    assert_eq!(response.body().unwrap().text(), "ok");
    assert!(Response::new(99, "", None).is_err());
}

#[test]
fn web_api_globals_install_and_run_through_runtime_builder() {
    let mut runtime = Runtime::builder()
        .global_classes(web_api_classes().iter().map(|class| class.spec))
        .build()
        .unwrap();
    let result = runtime
        .eval(SourceInput::from_javascript(
            r#"
            const url = new URL("https://example.com/a?x=1");
            const headers = new Headers();
            headers.append("Content-Type", " text/plain ");
            const blob = new Blob("hello", "TEXT/PLAIN");
            const request = new Request("https://example.com/api", "post", "body");
            const response = Response.json("{\"ok\":true}");
            url.href + "|" + headers.get("content-type") + "|" + blob.text() + "|" +
              request.method + "|" + response.status
            "#,
        ))
        .unwrap();
    assert_eq!(
        result.completion_string(),
        "https://example.com/a?x=1|text/plain|hello|POST|200"
    );
}
