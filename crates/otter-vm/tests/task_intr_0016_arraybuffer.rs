//! Integration tests for ES2024 ArrayBuffer (§25.1).
//!
//! Spec references:
//! - ArrayBuffer constructor: <https://tc39.es/ecma262/#sec-arraybuffer-constructor>
//! - get ArrayBuffer.prototype.byteLength:
//!   <https://tc39.es/ecma262/#sec-get-arraybuffer.prototype.bytelength>
//! - get ArrayBuffer.prototype.detached:
//!   <https://tc39.es/ecma262/#sec-get-arraybuffer.prototype.detached>
//! - get ArrayBuffer.prototype.maxByteLength:
//!   <https://tc39.es/ecma262/#sec-get-arraybuffer.prototype.maxbytelength>
//! - get ArrayBuffer.prototype.resizable:
//!   <https://tc39.es/ecma262/#sec-get-arraybuffer.prototype.resizable>
//! - ArrayBuffer.prototype.slice:
//!   <https://tc39.es/ecma262/#sec-arraybuffer.prototype.slice>
//! - ArrayBuffer.prototype.resize:
//!   <https://tc39.es/ecma262/#sec-arraybuffer.prototype.resize>
//! - ArrayBuffer.prototype.transfer:
//!   <https://tc39.es/ecma262/#sec-arraybuffer.prototype.transfer>
//! - ArrayBuffer.prototype.transferToFixedLength:
//!   <https://tc39.es/ecma262/#sec-arraybuffer.prototype.transfertofixedlength>
//! - ArrayBuffer.isView:
//!   <https://tc39.es/ecma262/#sec-arraybuffer.isview>
//! - get ArrayBuffer[@@species]:
//!   <https://tc39.es/ecma262/#sec-get-arraybuffer-%symbol.species%>

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

// ── §25.1.3.1 Constructor ────────────────────────────────────────────

#[test]
fn array_buffer_constructor_and_byte_length() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(8);\n",
            "assert.sameValue(buf.byteLength, 8, 'byteLength');\n",
            "assert.sameValue(Object.prototype.toString.call(buf), '[object ArrayBuffer]', 'toStringTag');\n",
        ),
        "arraybuffer-constructor.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn array_buffer_constructor_requires_new() {
    let r = run(
        concat!(
            "var threw = false;\n",
            "try { ArrayBuffer(8); } catch (e) { threw = e instanceof TypeError; }\n",
            "assert.sameValue(threw, true, 'ArrayBuffer without new throws');\n",
        ),
        "arraybuffer-new.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn array_buffer_constructor_to_index_semantics() {
    let r = run(
        concat!(
            "assert.sameValue(new ArrayBuffer().byteLength, 0, 'missing length -> 0');\n",
            "assert.sameValue(new ArrayBuffer(NaN).byteLength, 0, 'NaN -> 0');\n",
            "assert.sameValue(new ArrayBuffer(3.9).byteLength, 3, 'fraction truncates');\n",
            "assert.sameValue(new ArrayBuffer(-0.5).byteLength, 0, '-0.5 -> 0');\n",
            "var threw = false;\n",
            "try { new ArrayBuffer(-1); } catch (e) { threw = e instanceof RangeError; }\n",
            "assert.sameValue(threw, true, 'negative length throws');\n",
        ),
        "arraybuffer-toindex.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §25.1.3.1 Constructor with options (resizable) ──────────────────

#[test]
fn array_buffer_constructor_resizable() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(8, { maxByteLength: 16 });\n",
            "assert.sameValue(buf.byteLength, 8, 'initial byteLength');\n",
            "assert.sameValue(buf.resizable, true, 'resizable flag');\n",
            "assert.sameValue(buf.maxByteLength, 16, 'configured maxByteLength');\n",
            "assert.sameValue(buf.detached, false, 'not detached initially');\n",
        ),
        "arraybuffer-resizable.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn array_buffer_constructor_fixed_defaults() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(8);\n",
            "assert.sameValue(buf.resizable, false, 'fixed-length is not resizable');\n",
            "assert.sameValue(buf.maxByteLength, 8, 'fixed-length maxByteLength == byteLength');\n",
            "assert.sameValue(buf.detached, false, 'not detached');\n",
        ),
        "arraybuffer-fixed-defaults.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn array_buffer_constructor_max_less_than_length_throws() {
    let r = run(
        concat!(
            "var threw = false;\n",
            "try { new ArrayBuffer(16, { maxByteLength: 8 }); } catch (e) { threw = e instanceof RangeError; }\n",
            "assert.sameValue(threw, true, 'maxByteLength < byteLength throws RangeError');\n",
        ),
        "arraybuffer-max-less-than-length.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §25.1.5.4 ArrayBuffer.prototype.slice ────────────────────────────

#[test]
fn array_buffer_slice_clamps_indices() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(8);\n",
            "assert.sameValue(buf.slice(2).byteLength, 6, 'slice from start');\n",
            "assert.sameValue(buf.slice(2, 5).byteLength, 3, 'slice range');\n",
            "assert.sameValue(buf.slice(-3).byteLength, 3, 'negative start');\n",
            "assert.sameValue(buf.slice(1, 0 - 2).byteLength, 5, 'negative end');\n",
            "assert.sameValue(buf.slice(99).byteLength, 0, 'start past end clamps');\n",
            "assert.sameValue(buf.slice(6, 2).byteLength, 0, 'end before start gives empty');\n",
        ),
        "arraybuffer-slice.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn array_buffer_slice_returns_distinct_buffer() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(4);\n",
            "var sliced = buf.slice(0);\n",
            "assert.sameValue(buf === sliced, false, 'slice returns new buffer');\n",
        ),
        "arraybuffer-distinct.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §25.1.5.6 ArrayBuffer.prototype.resize ───────────────────────────

#[test]
fn array_buffer_resize_updates_byte_length() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(4, { maxByteLength: 12 });\n",
            "buf.resize(9);\n",
            "assert.sameValue(buf.byteLength, 9, 'resize updates byteLength');\n",
            "assert.sameValue(buf.maxByteLength, 12, 'maxByteLength unchanged after resize');\n",
            "buf.resize(9);\n",
            "assert.sameValue(buf.byteLength, 9, 'same-size resize is a no-op');\n",
            "buf.resize(2);\n",
            "assert.sameValue(buf.byteLength, 2, 'can shrink');\n",
        ),
        "arraybuffer-resize.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn array_buffer_resize_validates_constraints() {
    let r = run(
        concat!(
            "var fixed = new ArrayBuffer(4);\n",
            "var fixedThrew = false;\n",
            "try { fixed.resize(5); } catch (e) { fixedThrew = e instanceof TypeError; }\n",
            "assert.sameValue(fixedThrew, true, 'fixed-length AB cannot resize');\n",
            "var resizable = new ArrayBuffer(4, { maxByteLength: 6 });\n",
            "var tooLarge = false;\n",
            "try { resizable.resize(7); } catch (e) { tooLarge = e instanceof RangeError; }\n",
            "assert.sameValue(tooLarge, true, 'resize past max throws RangeError');\n",
        ),
        "arraybuffer-resize-errors.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §25.1.5.7 ArrayBuffer.prototype.transfer ─────────────────────────

#[test]
fn array_buffer_transfer_detaches_source() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(8);\n",
            "var buf2 = buf.transfer();\n",
            "assert.sameValue(buf.detached, true, 'source is detached after transfer');\n",
            "assert.sameValue(buf.byteLength, 0, 'detached byteLength is 0');\n",
            "assert.sameValue(buf2.byteLength, 8, 'new buffer has same byteLength');\n",
            "assert.sameValue(buf2.detached, false, 'new buffer is not detached');\n",
        ),
        "arraybuffer-transfer.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn array_buffer_transfer_with_new_length() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(8);\n",
            "var buf2 = buf.transfer(4);\n",
            "assert.sameValue(buf.detached, true, 'source detached');\n",
            "assert.sameValue(buf2.byteLength, 4, 'new buffer has requested length');\n",
            "var buf3 = new ArrayBuffer(4);\n",
            "var buf4 = buf3.transfer(16);\n",
            "assert.sameValue(buf4.byteLength, 16, 'transfer can grow');\n",
        ),
        "arraybuffer-transfer-length.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn array_buffer_transfer_preserves_resizability() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(8, { maxByteLength: 16 });\n",
            "var buf2 = buf.transfer();\n",
            "assert.sameValue(buf2.resizable, true, 'transfer preserves resizability');\n",
            "assert.sameValue(buf2.byteLength, 8, 'same byteLength');\n",
        ),
        "arraybuffer-transfer-resizable.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn array_buffer_transfer_detached_throws() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(8);\n",
            "buf.transfer();\n",
            "var threw = false;\n",
            "try { buf.transfer(); } catch (e) { threw = e instanceof TypeError; }\n",
            "assert.sameValue(threw, true, 'transfer on detached throws TypeError');\n",
        ),
        "arraybuffer-transfer-detached.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §25.1.5.8 ArrayBuffer.prototype.transferToFixedLength ────────────

#[test]
fn array_buffer_transfer_to_fixed_length() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(8, { maxByteLength: 16 });\n",
            "assert.sameValue(buf.resizable, true, 'source is resizable');\n",
            "var buf2 = buf.transferToFixedLength();\n",
            "assert.sameValue(buf.detached, true, 'source detached');\n",
            "assert.sameValue(buf2.resizable, false, 'result is fixed-length');\n",
            "assert.sameValue(buf2.byteLength, 8, 'same byteLength');\n",
        ),
        "arraybuffer-transfer-to-fixed.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §25.1.4.1 ArrayBuffer.isView ─────────────────────────────────────

#[test]
fn array_buffer_is_view_returns_false_for_non_views() {
    let r = run(
        concat!(
            "assert.sameValue(ArrayBuffer.isView({}), false, 'plain object');\n",
            "assert.sameValue(ArrayBuffer.isView(new ArrayBuffer(8)), false, 'ArrayBuffer itself');\n",
            "assert.sameValue(ArrayBuffer.isView(42), false, 'number');\n",
            "assert.sameValue(ArrayBuffer.isView(undefined), false, 'undefined');\n",
            "assert.sameValue(ArrayBuffer.isView(null), false, 'null');\n",
            "assert.sameValue(ArrayBuffer.isView('string'), false, 'string');\n",
        ),
        "arraybuffer-isview.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── §25.1.4.3 get ArrayBuffer[@@species] ─────────────────────────────

#[test]
fn array_buffer_species_getter() {
    let r = run(
        concat!(
            "assert.sameValue(ArrayBuffer[Symbol.species], ArrayBuffer, '@@species returns constructor');\n",
        ),
        "arraybuffer-species.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── Prototype validation ─────────────────────────────────────────────

#[test]
fn array_buffer_prototype_methods_validate_receiver() {
    let r = run(
        concat!(
            "var byteLengthThrew = false;\n",
            "try { ArrayBuffer.prototype.byteLength; } catch (e) { byteLengthThrew = e instanceof TypeError; }\n",
            "assert.sameValue(byteLengthThrew, true, 'byteLength getter validates receiver');\n",
            "var sliceThrew = false;\n",
            "try { ArrayBuffer.prototype.slice.call({}); } catch (e) { sliceThrew = e instanceof TypeError; }\n",
            "assert.sameValue(sliceThrew, true, 'slice validates receiver');\n",
        ),
        "arraybuffer-receiver.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

// ── Detached buffer edge cases ───────────────────────────────────────

#[test]
fn array_buffer_slice_on_detached_throws() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(8);\n",
            "buf.transfer();\n",
            "var threw = false;\n",
            "try { buf.slice(0); } catch (e) { threw = e instanceof TypeError; }\n",
            "assert.sameValue(threw, true, 'slice on detached throws TypeError');\n",
        ),
        "arraybuffer-slice-detached.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn array_buffer_resize_on_detached_throws() {
    let r = run(
        concat!(
            "var buf = new ArrayBuffer(8, { maxByteLength: 16 });\n",
            "buf.transfer();\n",
            "var threw = false;\n",
            "try { buf.resize(4); } catch (e) { threw = e instanceof TypeError; }\n",
            "assert.sameValue(threw, true, 'resize on detached throws TypeError');\n",
        ),
        "arraybuffer-resize-detached.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}
