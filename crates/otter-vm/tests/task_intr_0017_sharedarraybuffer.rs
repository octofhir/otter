//! Integration tests for ES2024 SharedArrayBuffer (§25.2).
//!
//! Spec references:
//! - SharedArrayBuffer constructor:
//!   <https://tc39.es/ecma262/#sec-sharedarraybuffer-constructor>
//! - get SharedArrayBuffer.prototype.byteLength:
//!   <https://tc39.es/ecma262/#sec-get-sharedarraybuffer.prototype.bytelength>
//! - SharedArrayBuffer.prototype.grow(newLength):
//!   <https://tc39.es/ecma262/#sec-sharedarraybuffer.prototype.grow>
//! - get SharedArrayBuffer.prototype.growable:
//!   <https://tc39.es/ecma262/#sec-get-sharedarraybuffer.prototype.growable>
//! - get SharedArrayBuffer.prototype.maxByteLength:
//!   <https://tc39.es/ecma262/#sec-get-sharedarraybuffer.prototype.maxbytelength>
//! - SharedArrayBuffer.prototype.slice(start, end):
//!   <https://tc39.es/ecma262/#sec-sharedarraybuffer.prototype.slice>

use otter_vm::source::compile_test262_basic_script;
use otter_vm::value::RegisterValue;
use otter_vm::{Interpreter, RuntimeState};

fn run(source: &str, url: &str) -> RegisterValue {
    let module = compile_test262_basic_script(source, url).expect("should compile");
    let mut runtime = RuntimeState::new();
    let global = runtime.intrinsics().global_object();
    let registers = [RegisterValue::from_object_handle(global.0)];
    Interpreter::new()
        .execute_with_runtime(
            &module,
            otter_vm::module::FunctionIndex(0),
            &registers,
            &mut runtime,
        )
        .expect("should execute")
        .return_value()
}

#[test]
fn shared_array_buffer_constructor_fixed_length_defaults() {
    let r = run(
        concat!(
            "var buf = new SharedArrayBuffer(8);\n",
            "assert.sameValue(buf.byteLength, 8, 'byteLength');\n",
            "assert.sameValue(buf.growable, false, 'fixed-length SAB is not growable');\n",
            "assert.sameValue(buf.maxByteLength, 8, 'fixed-length maxByteLength');\n",
            "assert.sameValue(Object.prototype.toString.call(buf), '[object SharedArrayBuffer]', 'toStringTag');\n",
            "assert.sameValue(SharedArrayBuffer[Symbol.species], SharedArrayBuffer, 'species getter');\n",
        ),
        "sharedarraybuffer-fixed.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn shared_array_buffer_constructor_requires_new() {
    let r = run(
        concat!(
            "var threw = false;\n",
            "try { SharedArrayBuffer(8); } catch (e) { threw = e instanceof TypeError; }\n",
            "assert.sameValue(threw, true, 'SharedArrayBuffer without new throws');\n",
        ),
        "sharedarraybuffer-new.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn shared_array_buffer_constructor_accepts_max_byte_length_option() {
    let r = run(
        concat!(
            "var buf = new SharedArrayBuffer(8, { maxByteLength: 16 });\n",
            "assert.sameValue(buf.byteLength, 8, 'initial byteLength');\n",
            "assert.sameValue(buf.growable, true, 'growable flag');\n",
            "assert.sameValue(buf.maxByteLength, 16, 'configured maxByteLength');\n",
        ),
        "sharedarraybuffer-options.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn shared_array_buffer_grow_updates_byte_length() {
    let r = run(
        concat!(
            "var buf = new SharedArrayBuffer(4, { maxByteLength: 12 });\n",
            "buf.grow(9);\n",
            "assert.sameValue(buf.byteLength, 9, 'grow updates byteLength');\n",
            "assert.sameValue(buf.maxByteLength, 12, 'maxByteLength remains configured maximum');\n",
            "buf.grow(9);\n",
            "assert.sameValue(buf.byteLength, 9, 'same-size grow is a no-op');\n",
        ),
        "sharedarraybuffer-grow.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn shared_array_buffer_grow_validates_length_and_growability() {
    let r = run(
        concat!(
            "var fixed = new SharedArrayBuffer(4);\n",
            "var fixedThrew = false;\n",
            "try { fixed.grow(5); } catch (e) { fixedThrew = e instanceof TypeError; }\n",
            "assert.sameValue(fixedThrew, true, 'fixed-length SAB cannot grow');\n",
            "var growable = new SharedArrayBuffer(4, { maxByteLength: 6 });\n",
            "var tooLarge = false;\n",
            "try { growable.grow(7); } catch (e) { tooLarge = e instanceof RangeError; }\n",
            "assert.sameValue(tooLarge, true, 'grow past max throws');\n",
            "growable.grow(6);\n",
            "var shrink = false;\n",
            "try { growable.grow(5); } catch (e) { shrink = e instanceof RangeError; }\n",
            "assert.sameValue(shrink, true, 'grow cannot shrink');\n",
        ),
        "sharedarraybuffer-grow-errors.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn shared_array_buffer_slice_returns_distinct_fixed_length_buffer() {
    let r = run(
        concat!(
            "var buf = new SharedArrayBuffer(8, { maxByteLength: 12 });\n",
            "var sliced = buf.slice(2, 0 - 1);\n",
            "assert.sameValue(buf === sliced, false, 'slice returns new buffer');\n",
            "assert.sameValue(sliced.byteLength, 5, 'slice clamps indices');\n",
            "assert.sameValue(sliced.growable, false, 'sliced buffer uses fixed-length constructor path');\n",
            "assert.sameValue(sliced.maxByteLength, 5, 'sliced fixed-length maxByteLength');\n",
            "assert.sameValue(Object.prototype.toString.call(sliced), '[object SharedArrayBuffer]', 'slice preserves toStringTag');\n",
        ),
        "sharedarraybuffer-slice.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn shared_array_buffer_prototype_methods_validate_receiver() {
    let r = run(
        concat!(
            "var byteLengthThrew = false;\n",
            "try { SharedArrayBuffer.prototype.byteLength; } catch (e) { byteLengthThrew = e instanceof TypeError; }\n",
            "assert.sameValue(byteLengthThrew, true, 'byteLength getter validates receiver');\n",
            "var growableThrew = false;\n",
            "try { SharedArrayBuffer.prototype.growable; } catch (e) { growableThrew = e instanceof TypeError; }\n",
            "assert.sameValue(growableThrew, true, 'growable getter validates receiver');\n",
            "var maxThrew = false;\n",
            "try { SharedArrayBuffer.prototype.maxByteLength; } catch (e) { maxThrew = e instanceof TypeError; }\n",
            "assert.sameValue(maxThrew, true, 'maxByteLength getter validates receiver');\n",
            "var growThrew = false;\n",
            "try { SharedArrayBuffer.prototype.grow.call({}); } catch (e) { growThrew = e instanceof TypeError; }\n",
            "assert.sameValue(growThrew, true, 'grow validates receiver');\n",
            "var sliceThrew = false;\n",
            "try { SharedArrayBuffer.prototype.slice.call({}); } catch (e) { sliceThrew = e instanceof TypeError; }\n",
            "assert.sameValue(sliceThrew, true, 'slice validates receiver');\n",
        ),
        "sharedarraybuffer-receiver.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}
