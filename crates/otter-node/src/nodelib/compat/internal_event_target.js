'use strict';
// The EventTarget internals `events.js` consults. The engine's EventTarget is
// the web one; its listener registry and listener limit live under the same
// registered symbols, the registry in Node's shape (type -> root
// `{ size, next }` heading a chain of handlers), so `getEventListeners`,
// `listenerCount` and `get`/`setMaxListeners` read them directly.
module.exports = {
  kResistStopPropagation: Symbol('kResistStopPropagation'),
  kEvents: Symbol.for('otter.EventTarget.events'),
  kMaxEventTargetListeners: Symbol.for('otter.EventTarget.maxListeners'),
  kMaxEventTargetListenersWarned: Symbol.for('otter.EventTarget.maxListenersWarned'),
  isEventTarget(value) {
    return typeof EventTarget === 'function' && value instanceof EventTarget;
  },
};
