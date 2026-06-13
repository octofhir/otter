'use strict';
// `node:stream/consumers` — collect a stream/iterable into a value.
const { Buffer } = require('buffer');

async function collect(stream) {
  const chunks = [];
  for await (const chunk of stream) chunks.push(chunk);
  return chunks;
}

async function buffer(stream) {
  const chunks = await collect(stream);
  return Buffer.concat(chunks.map((c) => (Buffer.isBuffer(c) ? c : Buffer.from(c))));
}
async function arrayBuffer(stream) {
  const buf = await buffer(stream);
  return buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength);
}
async function text(stream) {
  const buf = await buffer(stream);
  return buf.toString('utf8');
}
async function json(stream) {
  return JSON.parse(await text(stream));
}
async function blob(stream) {
  const buf = await buffer(stream);
  if (typeof Blob === 'function') return new Blob([buf]);
  return buf;
}

module.exports = { buffer, arrayBuffer, text, json, blob };
