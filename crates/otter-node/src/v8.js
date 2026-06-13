'use strict';
// `node:v8` — heap statistics + serialize/deserialize subset.
const { Buffer } = require('buffer');

function getHeapStatistics() {
  return {
    total_heap_size: 0, total_heap_size_executable: 0, total_physical_size: 0,
    total_available_size: 0, used_heap_size: 0, heap_size_limit: 2197815296,
    malloced_memory: 0, peak_malloced_memory: 0, does_zap_garbage: 0,
    number_of_native_contexts: 1, number_of_detached_contexts: 0,
    total_global_handles_size: 0, used_global_handles_size: 0, external_memory: 0,
  };
}
function getHeapSpaceStatistics() {
  return ['read_only_space', 'new_space', 'old_space', 'code_space', 'map_space', 'large_object_space']
    .map((space_name) => ({ space_name, space_size: 0, space_used_size: 0, space_available_size: 0, physical_space_size: 0 }));
}
function getHeapCodeStatistics() {
  return { code_and_metadata_size: 0, bytecode_and_metadata_size: 0, external_script_source_size: 0, cpu_profiler_metadata_size: 0 };
}
function setFlagsFromString() {}
function cachedDataVersionTag() { return 0; }
function takeCoverage() {}
function stopCoverage() {}
function setHeapSnapshotNearHeapLimit() {}
function writeHeapSnapshot() { return ''; }

// JSON-based serialize/deserialize (sufficient for JSON-able round trips).
function serialize(value) { return Buffer.from(JSON.stringify(value === undefined ? null : value)); }
function deserialize(buffer) { return JSON.parse(Buffer.from(buffer).toString('utf8')); }

class Serializer {
  constructor() { this._chunks = []; }
  writeHeader() {}
  writeValue(value) { this._chunks.push(JSON.stringify(value)); }
  releaseBuffer() { return Buffer.from(this._chunks.join('')); }
  writeUint32() {} writeUint64() {} writeDouble() {} writeRawBytes() {}
  _setTreatArrayBufferViewsAsHostObjects() {}
}
class Deserializer {
  constructor(buffer) { this._buffer = buffer; }
  readHeader() { return true; }
  readValue() { return JSON.parse(Buffer.from(this._buffer).toString('utf8')); }
  readUint32() { return 0; } readUint64() { return 0; } readDouble() { return 0; } readRawBytes() { return Buffer.alloc(0); }
}

module.exports = {
  getHeapStatistics, getHeapSpaceStatistics, getHeapCodeStatistics,
  setFlagsFromString, cachedDataVersionTag, takeCoverage, stopCoverage,
  setHeapSnapshotNearHeapLimit, writeHeapSnapshot,
  serialize, deserialize,
  Serializer, Deserializer, DefaultSerializer: Serializer, DefaultDeserializer: Deserializer,
  promiseHooks: { createHook() { return { enable() {}, disable() {} }; }, onInit() {}, onBefore() {}, onAfter() {}, onSettled() {} },
  startupSnapshot: { addSerializeCallback() {}, addDeserializeCallback() {}, setDeserializeMainFunction() {}, isBuildingSnapshot() { return false; } },
};
