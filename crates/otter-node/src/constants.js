'use strict';
// `node:constants` — the legacy aggregate: os signal/errno constants, fs
// constants, and crypto constants folded into one namespace, exactly the
// shape Node's deprecated `constants` module presents.
const os = require('os');
const fs = require('fs');

const out = {};
for (const source of [os.constants.errno, os.constants.signals, fs.constants]) {
  if (source === undefined) continue;
  for (const key of Object.keys(source)) out[key] = source[key];
}
if (os.constants.priority !== undefined) {
  for (const key of Object.keys(os.constants.priority)) {
    out[key] = os.constants.priority[key];
  }
}
module.exports = out;
