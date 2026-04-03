use otter_runtime::{ModuleLoaderConfig, OtterRuntime, RegisterValue};
use otter_web::web_extension;

fn global_property(runtime: &mut OtterRuntime, name: &str) -> RegisterValue {
    let global = runtime.state_mut().intrinsics().global_object();
    let property = runtime.state_mut().intern_property_name(name);
    runtime
        .state_mut()
        .own_property_value(global, property)
        .expect("global property should exist")
}

fn global_string_property(runtime: &mut OtterRuntime, name: &str) -> String {
    let value = global_property(runtime, name);
    runtime
        .state_mut()
        .js_to_string_infallible(value)
        .into_string()
}

fn configure_runtime() -> OtterRuntime {
    OtterRuntime::builder()
        .module_loader(ModuleLoaderConfig::default())
        .extension(web_extension())
        .build()
}

fn assert_global_error(runtime: &mut OtterRuntime) {
    let error = global_property(runtime, "__error");
    if error != RegisterValue::undefined() {
        let message = runtime
            .state_mut()
            .js_to_string_infallible(error)
            .into_string();
        let step = global_property(runtime, "__step")
            .as_number()
            .unwrap_or(-1.0);
        panic!("script failed at step {step}: {message}");
    }
}

#[test]
fn blob_and_file_follow_core_web_api_behavior() {
    let mut runtime = configure_runtime();
    runtime
        .run_script(
            "var __step = 0; \
             var __error = undefined; \
             try { \
               __step = 1; \
               const blob = new Blob(['Hello', new Uint8Array([32, 79, 116, 116, 101, 114])], { type: 'Text/Plain' }); \
               __step = 2; \
               const slice = blob.slice(6, -1, 'APPLICATION/JSON'); \
               __step = 3; \
               const file = new File([blob], 'greet.txt', { type: 'text/custom', lastModified: 123 }); \
               __step = 4; \
               var __blobSize = blob.size; \
               __step = 5; \
               var __blobType = blob.type; \
               __step = 6; \
               var __sliceType = slice.type; \
               __step = 7; \
               var __fileName = file.name; \
               __step = 8; \
               var __fileType = file.type; \
               __step = 9; \
               var __fileLastModified = file.lastModified; \
               __step = 10; \
               var __fileIsBlob = file instanceof Blob; \
               __step = 11; \
               var __fileIsFile = file instanceof File; \
               __step = 12; \
              } catch (error) { \
               __error = error; \
              }",
            "main.js",
        )
        .expect("blob/file script should execute");

    assert_global_error(&mut runtime);
    assert_eq!(
        global_property(&mut runtime, "__blobSize"),
        RegisterValue::from_i32(11)
    );
    assert_eq!(
        global_string_property(&mut runtime, "__blobType"),
        "text/plain"
    );
    assert_eq!(
        global_string_property(&mut runtime, "__sliceType"),
        "application/json"
    );
    assert_eq!(
        global_string_property(&mut runtime, "__fileName"),
        "greet.txt"
    );
    assert_eq!(
        global_string_property(&mut runtime, "__fileType"),
        "text/custom"
    );
    assert_eq!(
        global_property(&mut runtime, "__fileLastModified"),
        RegisterValue::from_i32(123)
    );
    assert_eq!(
        global_property(&mut runtime, "__fileIsBlob"),
        RegisterValue::from_bool(true)
    );
    assert_eq!(
        global_property(&mut runtime, "__fileIsFile"),
        RegisterValue::from_bool(true)
    );
}

#[test]
fn form_data_supports_strings_and_blob_entries() {
    let mut runtime = configure_runtime();
    runtime
        .run_script(
            "var __step = 0; \
             var __error = undefined; \
             try { \
               __step = 1; \
               const data = new FormData(); \
               __step = 2; \
               data.append('tag', 'one'); \
               __step = 3; \
               data.append('tag', 'two'); \
               __step = 4; \
               data.append('upload', new Blob(['payload'], { type: 'text/plain' }), 'payload.txt'); \
               __step = 5; \
               data.set('doc', new File(['report'], 'report.txt', { lastModified: 77 }), 'renamed.txt'); \
               __step = 6; \
               const tags = data.getAll('tag'); \
               __step = 7; \
               const upload = data.get('upload'); \
               __step = 8; \
               const doc = data.get('doc'); \
               __step = 9; \
               var __firstTag = data.get('tag'); \
               __step = 10; \
               var __secondTag = tags[1]; \
               __step = 11; \
               var __tagCount = tags.length; \
               __step = 12; \
               var __hasTagBeforeDelete = data.has('tag'); \
               __step = 13; \
               var __uploadIsFile = upload instanceof File; \
               __step = 14; \
               var __uploadName = upload.name; \
               __step = 15; \
               var __docIsFile = doc instanceof File; \
               __step = 16; \
               var __docName = doc.name; \
               __step = 17; \
               var __docLastModified = doc.lastModified; \
               __step = 18; \
               data.delete('tag'); \
               var __hasTagAfterDelete = data.has('tag'); \
              } catch (error) { \
               __error = error; \
              }",
            "main.js",
        )
        .expect("form data script should execute");

    assert_global_error(&mut runtime);
    assert_eq!(global_string_property(&mut runtime, "__firstTag"), "one");
    assert_eq!(global_string_property(&mut runtime, "__secondTag"), "two");
    assert_eq!(
        global_property(&mut runtime, "__tagCount"),
        RegisterValue::from_i32(2)
    );
    assert_eq!(
        global_property(&mut runtime, "__hasTagBeforeDelete"),
        RegisterValue::from_bool(true)
    );
    assert_eq!(
        global_property(&mut runtime, "__hasTagAfterDelete"),
        RegisterValue::from_bool(false)
    );
    assert_eq!(
        global_string_property(&mut runtime, "__docName"),
        "renamed.txt"
    );
    assert_eq!(
        global_property(&mut runtime, "__docIsFile"),
        RegisterValue::from_bool(true)
    );
    assert_eq!(
        global_string_property(&mut runtime, "__uploadName"),
        "payload.txt"
    );
    assert_eq!(
        global_property(&mut runtime, "__uploadIsFile"),
        RegisterValue::from_bool(true)
    );
    assert_eq!(
        global_property(&mut runtime, "__docLastModified"),
        RegisterValue::from_i32(77)
    );
}
