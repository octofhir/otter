'use strict';

// `node:timers`, wired to the loop.
//
// Node's own `lib/timers.js` is the module; what it cannot do by itself is
// arrange for its queues to be drained. In Node the loop does that: a timer
// handle calls back when the next expiry arrives, and a check handle runs the
// immediate queue every turn. Here the engine's timers stand in for both, and
// nothing tells them an immediate was queued — so queueing one says so.

const binding = require('internal/otter/timers_binding');
const timers = require('internal/otter/timers');

{
  const { getTimerCallbacks } = require('internal/timers');
  // Node drains its tick queue between two timer callbacks. This engine keeps
  // ticks in the microtask queue, which drains when the turn that ran them
  // ends, so there is nothing to drain in between.
  const runNextTicks = () => {};
  const { processImmediate, processTimers } = getTimerCallbacks(runNextTicks);
  binding.setupTimers(processImmediate, processTimers);
}

// Keep everything the module says about a function it exports — the
// promisified form rides on a symbol, and callers compare it by identity.
function carrying(original, replacement) {
  for (const key of Reflect.ownKeys(original)) {
    const desc = Reflect.getOwnPropertyDescriptor(original, key);
    Reflect.defineProperty(replacement, key, desc);
  }
  return replacement;
}

// The module's own exports object is what is handed on, with the one function
// replaced in place. Copying the object instead would read its lazily built
// `promises`, which asks for this module back before it exists.
{
  const original = timers.setImmediate;
  timers.setImmediate = carrying(original, function setImmediate(callback, ...args) {
    const immediate = original(callback, ...args);
    binding.armCheckPhase();
    return immediate;
  });
}

module.exports = timers;
