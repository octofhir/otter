'use strict';

// `internal/worker/js_transferable` — transfer-mode branding without a
// worker messaging channel. Marking is a no-op beyond stashing the mode;
// the symbols exist so classes can define their serialize hooks.

const kTransfer = Symbol('kTransfer');
const kTransferList = Symbol('kTransferList');
const kDeserialize = Symbol('kDeserialize');
const kClone = Symbol('kClone');
const kTransferMode = Symbol('kTransferMode');

function markTransferMode(object, cloneable = false, transferable = false) {
  if (object === null || typeof object !== 'object') return;
  object[kTransferMode] = { cloneable, transferable };
}

module.exports = {
  markTransferMode,
  kTransfer,
  kTransferList,
  kDeserialize,
  kClone,
};
