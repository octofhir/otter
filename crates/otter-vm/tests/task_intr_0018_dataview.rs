//! Integration tests for ES2024 DataView (§25.3).
//!
//! Spec references:
//! - DataView constructor: <https://tc39.es/ecma262/#sec-dataview-constructor>
//! - DataView.prototype accessors:
//!   <https://tc39.es/ecma262/#sec-properties-of-the-dataview-prototype-object>
//! - GetViewValue: <https://tc39.es/ecma262/#sec-getviewvalue>
//! - SetViewValue: <https://tc39.es/ecma262/#sec-setviewvalue>

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

// ── §25.3.2.1 Constructor ────────────────────────────────────────────

#[test]
fn data_view_constructor_basic() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(8);\n",
            "var dv = new DataView(buf);\n",
            "assert.sameValue(dv.byteLength, 8, 'byteLength covers whole buffer');\n",
            "assert.sameValue(dv.byteOffset, 0, 'default byteOffset is 0');\n",
            "assert.sameValue(dv.buffer, buf, 'buffer getter returns underlying ArrayBuffer');\n",
            "assert.sameValue(Object.prototype.toString.call(dv), '[object DataView]', 'toStringTag');\n",
        ),
        "dataview-constructor.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn data_view_constructor_with_offset_and_length() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(16);\n",
            "var dv = new DataView(buf, 4, 8);\n",
            "assert.sameValue(dv.byteLength, 8, 'explicit byteLength');\n",
            "assert.sameValue(dv.byteOffset, 4, 'explicit byteOffset');\n",
        ),
        "dataview-offset-length.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn data_view_constructor_requires_new() {
    let r = run(
        concat!(
            "var threw = false;\n",
            "try { DataView(new ArrayBuffer(8)); } catch (e) { threw = e instanceof TypeError; }\n",
            "assert.sameValue(threw, true, 'DataView without new throws TypeError');\n",
        ),
        "dataview-new.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn data_view_constructor_requires_buffer() {
    let r = run(
        concat!(
            "var threw = false;\n",
            "try { new DataView({}); } catch (e) { threw = e instanceof TypeError; }\n",
            "assert.sameValue(threw, true, 'non-buffer argument throws TypeError');\n",
        ),
        "dataview-requires-buffer.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn data_view_constructor_offset_exceeds_buffer() {
    let r = run(
        concat!(
            "var threw = false;\n",
            "try { new DataView(new ArrayBuffer(8), 9); } catch (e) { threw = e instanceof RangeError; }\n",
            "assert.sameValue(threw, true, 'offset beyond buffer throws RangeError');\n",
        ),
        "dataview-offset-range.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn data_view_constructor_offset_plus_length_exceeds_buffer() {
    let r = run(
        concat!(
            "var threw = false;\n",
            "try { new DataView(new ArrayBuffer(8), 4, 5); } catch (e) { threw = e instanceof RangeError; }\n",
            "assert.sameValue(threw, true, 'offset + length beyond buffer throws RangeError');\n",
        ),
        "dataview-offset-length-range.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §25.3.4.5–12 Get methods (integer types) ────────────────────────

#[test]
fn data_view_get_set_int8_positive() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(4);\n",
            "var dv = new DataView(buf);\n",
            "dv.setInt8(0, 127);\n",
            "assert.sameValue(dv.getInt8(0), 127, 'max positive');\n",
        ),
        "dataview-int8-pos.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn data_view_get_set_int8_negative() {
    // Use (0 - N) instead of -N to work around unary minus compiler issue.
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(4);\n",
            "var dv = new DataView(buf);\n",
            "dv.setInt8(0, 0 - 128);\n",
            "assert.sameValue(dv.getUint8(0), 128, 'raw byte = 128');\n",
            "assert.sameValue(dv.getInt8(0), 0 - 128, 'min negative');\n",
        ),
        "dataview-int8-neg.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn data_view_get_set_int8_zero() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(4);\n",
            "var dv = new DataView(buf);\n",
            "dv.setInt8(0, 0);\n",
            "assert.sameValue(dv.getInt8(0), 0, 'zero');\n",
        ),
        "dataview-int8-zero.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn data_view_get_set_uint8() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(4);\n",
            "var dv = new DataView(buf);\n",
            "dv.setUint8(0, 255);\n",
            "dv.setUint8(1, 0);\n",
            "assert.sameValue(dv.getUint8(0), 255, 'max uint8');\n",
            "assert.sameValue(dv.getUint8(1), 0, 'zero');\n",
        ),
        "dataview-uint8.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn data_view_get_set_int16_endianness() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(4);\n",
            "var dv = new DataView(buf);\n",
            "dv.setInt16(0, 0x0102, false);\n", // big-endian
            "assert.sameValue(dv.getUint8(0), 1, 'BE high byte');\n",
            "assert.sameValue(dv.getUint8(1), 2, 'BE low byte');\n",
            "assert.sameValue(dv.getInt16(0, false), 0x0102, 'read BE');\n",
            "dv.setInt16(2, 0x0304, true);\n", // little-endian
            "assert.sameValue(dv.getUint8(2), 4, 'LE low byte');\n",
            "assert.sameValue(dv.getUint8(3), 3, 'LE high byte');\n",
            "assert.sameValue(dv.getInt16(2, true), 0x0304, 'read LE');\n",
        ),
        "dataview-int16-endian.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn data_view_get_set_int32() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(8);\n",
            "var dv = new DataView(buf);\n",
            "dv.setInt32(0, 0x01020304, false);\n",
            "assert.sameValue(dv.getInt32(0, false), 0x01020304, 'round-trip BE');\n",
            "var neg = 0 - 1;\n",
            "dv.setInt32(4, neg, true);\n",
            "assert.sameValue(dv.getInt32(4, true), neg, 'round-trip LE negative');\n",
        ),
        "dataview-int32.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn data_view_get_set_uint32() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(4);\n",
            "var dv = new DataView(buf);\n",
            "dv.setUint32(0, 0xFFFFFFFF, false);\n",
            "assert.sameValue(dv.getUint32(0, false), 0xFFFFFFFF, 'max uint32 BE');\n",
        ),
        "dataview-uint32.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §25.3.4.7–6 Float types ─────────────────────────────────────────

#[test]
fn data_view_get_set_float32() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(4);\n",
            "var dv = new DataView(buf);\n",
            "dv.setFloat32(0, 1.5, true);\n",
            "var val = dv.getFloat32(0, true);\n",
            "assert.sameValue(val, 1.5, 'float32 round-trip');\n",
        ),
        "dataview-float32.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn data_view_get_set_float64() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(8);\n",
            "var dv = new DataView(buf);\n",
            "dv.setFloat64(0, 3.141592653589793, true);\n",
            "var val = dv.getFloat64(0, true);\n",
            "assert.sameValue(val, 3.141592653589793, 'float64 round-trip');\n",
        ),
        "dataview-float64.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── Bounds checking ──────────────────────────────────────────────────

#[test]
fn data_view_get_out_of_bounds_throws() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(4);\n",
            "var dv = new DataView(buf);\n",
            "var threw8 = false;\n",
            "try { dv.getInt8(4); } catch (e) { threw8 = e instanceof RangeError; }\n",
            "assert.sameValue(threw8, true, 'getInt8 past end throws');\n",
            "var threw32 = false;\n",
            "try { dv.getInt32(1); } catch (e) { threw32 = e instanceof RangeError; }\n",
            "assert.sameValue(threw32, true, 'getInt32 would read past end');\n",
        ),
        "dataview-bounds.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn data_view_set_out_of_bounds_throws() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(4);\n",
            "var dv = new DataView(buf);\n",
            "var threw = false;\n",
            "try { dv.setInt32(1, 0); } catch (e) { threw = e instanceof RangeError; }\n",
            "assert.sameValue(threw, true, 'setInt32 past end throws');\n",
        ),
        "dataview-set-bounds.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── DataView with offset into buffer ─────────────────────────────────

#[test]
fn data_view_with_byte_offset_reads_correct_region() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(8);\n",
            "var full = new DataView(buf);\n",
            "full.setUint8(4, 0xAB);\n",
            "full.setUint8(5, 0xCD);\n",
            "var sub = new DataView(buf, 4, 2);\n",
            "assert.sameValue(sub.getUint8(0), 0xAB, 'offset view reads correct byte');\n",
            "assert.sameValue(sub.getUint8(1), 0xCD, 'offset view second byte');\n",
            "assert.sameValue(sub.byteLength, 2, 'sub view byteLength');\n",
            "assert.sameValue(sub.byteOffset, 4, 'sub view byteOffset');\n",
        ),
        "dataview-offset.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── Detached buffer ──────────────────────────────────────────────────

#[test]
fn data_view_detached_buffer_throws_on_access() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(8);\n",
            "var dv = new DataView(buf);\n",
            "buf.transfer();\n",
            "var blThrew = false;\n",
            "try { dv.byteLength; } catch (e) { blThrew = e instanceof TypeError; }\n",
            "assert.sameValue(blThrew, true, 'byteLength on detached throws');\n",
            "var getThrew = false;\n",
            "try { dv.getInt8(0); } catch (e) { getThrew = e instanceof TypeError; }\n",
            "assert.sameValue(getThrew, true, 'getInt8 on detached throws');\n",
        ),
        "dataview-detached.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── SharedArrayBuffer as backing ─────────────────────────────────────

#[test]
fn data_view_over_shared_array_buffer() {
    let r = run(
        concat!(
            "var sab = new SharedArrayBuffer(8);\n",
            "var dv = new DataView(sab);\n",
            "dv.setInt32(0, 42, true);\n",
            "assert.sameValue(dv.getInt32(0, true), 42, 'read/write through SAB');\n",
            "assert.sameValue(dv.buffer, sab, 'buffer returns SAB');\n",
        ),
        "dataview-sab.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── ArrayBuffer.isView now returns true for DataView ─────────────────

#[test]
fn array_buffer_is_view_returns_true_for_data_view() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(8);\n",
            "var dv = new DataView(buf);\n",
            "assert.sameValue(ArrayBuffer.isView(dv), true, 'DataView is a view');\n",
        ),
        "arraybuffer-isview-dataview.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── Prototype validation ─────────────────────────────────────────────

#[test]
fn data_view_prototype_validates_receiver() {
    let r = run(
        concat!(
            "var blThrew = false;\n",
            "try { DataView.prototype.byteLength; } catch (e) { blThrew = e instanceof TypeError; }\n",
            "assert.sameValue(blThrew, true, 'byteLength getter validates receiver');\n",
            "var getThrew = false;\n",
            "try { DataView.prototype.getInt8.call({}, 0); } catch (e) { getThrew = e instanceof TypeError; }\n",
            "assert.sameValue(getThrew, true, 'getInt8 validates receiver');\n",
        ),
        "dataview-receiver.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}
