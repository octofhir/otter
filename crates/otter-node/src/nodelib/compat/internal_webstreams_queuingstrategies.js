'use strict';

// internal/webstreams/queuingstrategies — the standard strategies, which this
// realm already has as globals.

const { ByteLengthQueuingStrategy, CountQueuingStrategy } = globalThis;

module.exports = { ByteLengthQueuingStrategy, CountQueuingStrategy };
