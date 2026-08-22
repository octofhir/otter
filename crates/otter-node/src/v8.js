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

// ---- structured serialization ----
//
// Both ends of this are this runtime: the test runner writes a message in a
// child process and reads it in the parent, and nothing outside reads what
// travels between them. So the encoding only has to be faithful to itself and
// to what a message carries — which is more than JSON alone: a serialized
// error arrives as a `Buffer`, a report may carry `undefined`, and a graph may
// point back at itself.

const kRef = '$otterRef';
const kTag = '$otter';

function encodeValue(value, seen) {
  if (value === undefined) return { [kTag]: 'undefined' };
  if (typeof value === 'bigint') return { [kTag]: 'bigint', v: String(value) };
  if (value === null || typeof value !== 'object') return value;

  const existing = seen.get(value);
  if (existing !== undefined) return { [kRef]: existing };
  const id = seen.size;
  seen.set(value, id);

  if (ArrayBuffer.isView(value)) {
    const bytes = new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
    return { [kTag]: 'bytes', id, kind: value.constructor?.name ?? 'Uint8Array', v: Array.from(bytes) };
  }
  if (value instanceof ArrayBuffer) {
    return { [kTag]: 'arraybuffer', id, v: Array.from(new Uint8Array(value)) };
  }
  if (value instanceof Date) return { [kTag]: 'date', id, v: value.getTime() };
  if (value instanceof RegExp) return { [kTag]: 'regexp', id, source: value.source, flags: value.flags };
  if (value instanceof Map) {
    return { [kTag]: 'map', id, v: Array.from(value, ([k, val]) => [encodeValue(k, seen), encodeValue(val, seen)]) };
  }
  if (value instanceof Set) {
    return { [kTag]: 'set', id, v: Array.from(value, (item) => encodeValue(item, seen)) };
  }
  if (Array.isArray(value)) {
    return { [kTag]: 'array', id, v: value.map((item) => encodeValue(item, seen)) };
  }
  const entries = [];
  for (const key of Object.keys(value)) entries.push([key, encodeValue(value[key], seen)]);
  return { [kTag]: 'object', id, v: entries };
}

function decodeValue(node, byId) {
  if (node === null || typeof node !== 'object') return node;
  if (kRef in node) return byId.get(node[kRef]);
  const tag = node[kTag];
  if (tag === undefined) return node;
  switch (tag) {
    case 'undefined': return undefined;
    case 'bigint': return BigInt(node.v);
    case 'bytes': {
      const bytes = Uint8Array.from(node.v);
      const built = node.kind === 'Buffer' ? Buffer.from(bytes) : bytes;
      byId.set(node.id, built);
      return built;
    }
    case 'arraybuffer': {
      const built = Uint8Array.from(node.v).buffer;
      byId.set(node.id, built);
      return built;
    }
    case 'date': { const built = new Date(node.v); byId.set(node.id, built); return built; }
    case 'regexp': { const built = new RegExp(node.source, node.flags); byId.set(node.id, built); return built; }
    case 'map': {
      const built = new Map();
      byId.set(node.id, built);
      for (const [k, v] of node.v) built.set(decodeValue(k, byId), decodeValue(v, byId));
      return built;
    }
    case 'set': {
      const built = new Set();
      byId.set(node.id, built);
      for (const item of node.v) built.add(decodeValue(item, byId));
      return built;
    }
    case 'array': {
      const built = [];
      byId.set(node.id, built);
      for (const item of node.v) built.push(decodeValue(item, byId));
      return built;
    }
    case 'object': {
      const built = {};
      byId.set(node.id, built);
      for (const [key, item] of node.v) built[key] = decodeValue(item, byId);
      return built;
    }
    default: return node;
  }
}

function encode(value) { return JSON.stringify(encodeValue(value, new Map())); }
function decode(text) { return decodeValue(JSON.parse(text), new Map()); }

// The header is a marker, not decoration: a reader pulling serialized messages
// out of a stream that also carries the program's own output finds where one
// begins by looking for it. It has to be bytes text does not ordinarily
// contain, and it has to be there.
const kHeader = Buffer.from([0xFF, 0x0F]);

function serialize(value) {
  return Buffer.concat([kHeader, Buffer.from(encode(value), 'utf8')]);
}

function bodyOf(buffer) {
  const bytes = Buffer.from(buffer);
  const headed = bytes.length >= kHeader.length &&
    bytes[0] === kHeader[0] && bytes[1] === kHeader[1];
  return headed ? bytes.subarray(kHeader.length) : bytes;
}

function deserialize(buffer) { return decode(bodyOf(buffer).toString('utf8')); }

class Serializer {
  constructor() { this._chunks = []; }
  writeHeader() { this._chunks.push(Buffer.from(kHeader)); }
  writeValue(value) { this._chunks.push(Buffer.from(encode(value), 'utf8')); }
  writeRawBytes(bytes) { this._chunks.push(Buffer.from(bytes)); }
  releaseBuffer() {
    const buffer = Buffer.concat(this._chunks);
    this._chunks = [];
    return buffer;
  }
  writeUint32() {} writeUint64() {} writeDouble() {}
  _setTreatArrayBufferViewsAsHostObjects() {}
}

class Deserializer {
  constructor(buffer) {
    this._buffer = Buffer.from(buffer ?? []);
    this._at = 0;
  }
  readHeader() {
    const bytes = this._buffer;
    if (bytes.length - this._at >= kHeader.length &&
        bytes[this._at] === kHeader[0] && bytes[this._at + 1] === kHeader[1]) {
      this._at += kHeader.length;
    }
    return true;
  }
  readValue() { return decode(this._buffer.subarray(this._at).toString('utf8')); }
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
