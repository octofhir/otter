'use strict';
const { Buffer } = require('buffer');
function FastBuffer(arrayBuffer, byteOffset, length) {
  return Buffer.from(arrayBuffer, byteOffset, length);
}
module.exports = { FastBuffer };
