'use strict';
// The EventTarget internals `events.js` consults. The engine's EventTarget is
// the web one; its listener registry lives under the same registered symbol
// and carries Node's shape (type -> root `{ size, next }` heading a chain of
// handlers), so `getEventListeners`/`listenerCount` read it directly.
module.exports = {
  kResistStopPropagation: Symbol('kResistStopPropagation'),
  kEvents: Symbol.for('otter.EventTarget.events'),
  kMaxEventTargetListeners: Symbol('events.maxEventTargetListeners'),
  kMaxEventTargetListenersWarned: Symbol('events.maxEventTargetListenersWarned'),
  isEventTarget(value) {
    return typeof EventTarget === 'function' && value instanceof EventTarget;
  },
};
