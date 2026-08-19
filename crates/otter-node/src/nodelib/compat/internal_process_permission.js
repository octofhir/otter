'use strict';
// internal/process/permission — the permission model Node's `--permission`
// flag turns on. This runtime gates file and network access through its own
// capability set instead, so the model reports itself as not enabled and
// every check answers `true`; the capability gate is what refuses.
module.exports = {
  isEnabled() { return false; },
  has() { return true; },
};
