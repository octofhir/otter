'use strict';

// Node's timer lists, with one thing added: queueing an immediate says so.
//
// In Node the loop reaches its check phase every turn and drains the immediate
// queue whether or not anything was put on it. Here the check phase is one
// engine immediate that has to be armed, and the only moment that is known is
// when an `Immediate` is constructed — which is what appends it to the queue.
const lists = require('internal/otter/timers_lists');
const binding = require('internal/otter/timers_binding');

class Immediate extends lists.Immediate {
  constructor(...args) {
    super(...args);
    binding.armCheckPhase();
  }
}

module.exports = { ...lists, Immediate };
