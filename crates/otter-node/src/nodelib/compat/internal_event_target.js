'use strict';
// The EventTarget internals `events.js` consults. The engine's EventTarget
// is the web one, without Node's kEvents bookkeeping; the symbol lookups
// answer `undefined` and the callers' optional chains handle it.
module.exports = {
  kResistStopPropagation: Symbol('kResistStopPropagation'),
  kEvents: Symbol('kEvents'),
  kMaxEventTargetListeners: Symbol('events.maxEventTargetListeners'),
  kMaxEventTargetListenersWarned: Symbol('events.maxEventTargetListenersWarned'),
  isEventTarget(value) {
    return typeof EventTarget === 'function' && value instanceof EventTarget;
  },
};
