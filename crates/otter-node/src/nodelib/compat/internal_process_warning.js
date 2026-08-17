'use strict';
module.exports = {
  emitWarningSync: (...args) => process.emitWarning(...args),
  onWarning() {},
};
