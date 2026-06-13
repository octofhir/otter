'use strict';
// `node:perf_hooks` — performance timeline subset. Provides the `performance`
// object (also published as a global) plus PerformanceObserver/entries stubs.

const timeOrigin = Date.now();
let entries = [];

class PerformanceEntry {
  constructor(name, entryType, startTime, duration, detail) {
    this.name = name;
    this.entryType = entryType;
    this.startTime = startTime;
    this.duration = duration || 0;
    this.detail = detail;
  }
  toJSON() { return { name: this.name, entryType: this.entryType, startTime: this.startTime, duration: this.duration }; }
}
class PerformanceMark extends PerformanceEntry {
  constructor(name, options) { super(name, 'mark', Date.now() - timeOrigin, 0, options && options.detail); }
}
class PerformanceMeasure extends PerformanceEntry {
  constructor(name, start, duration, detail) { super(name, 'measure', start, duration, detail); }
}

const marks = new Map();

const performance = (typeof globalThis !== 'undefined' && globalThis.performance) || {
  timeOrigin,
  now() { return Date.now() - timeOrigin; },
  mark(name, options) {
    const m = new PerformanceMark(name, options);
    marks.set(name, m);
    entries.push(m);
    return m;
  },
  measure(name, startOrOptions, endMark) {
    let start = 0; let end = this.now();
    if (typeof startOrOptions === 'string') {
      const s = marks.get(startOrOptions); if (s) start = s.startTime;
      if (endMark) { const e = marks.get(endMark); if (e) end = e.startTime; }
    } else if (startOrOptions && typeof startOrOptions === 'object') {
      if (typeof startOrOptions.start === 'number') start = startOrOptions.start;
      if (typeof startOrOptions.end === 'number') end = startOrOptions.end;
      if (typeof startOrOptions.duration === 'number') end = start + startOrOptions.duration;
    }
    const m = new PerformanceMeasure(name, start, end - start, startOrOptions && startOrOptions.detail);
    entries.push(m);
    return m;
  },
  getEntries() { return entries.slice(); },
  getEntriesByName(name, type) { return entries.filter((e) => e.name === name && (!type || e.entryType === type)); },
  getEntriesByType(type) { return entries.filter((e) => e.entryType === type); },
  clearMarks(name) { entries = entries.filter((e) => e.entryType !== 'mark' || (name && e.name !== name)); if (name) marks.delete(name); else marks.clear(); },
  clearMeasures(name) { entries = entries.filter((e) => e.entryType !== 'measure' || (name && e.name !== name)); },
  clearResourceTimings() {},
  markResourceTiming() {},
  setResourceTimingBufferSize() {},
  eventLoopUtilization() { return { idle: 0, active: 0, utilization: 0 }; },
  toJSON() { return { timeOrigin, now: this.now() }; },
  nodeTiming: { name: 'node', entryType: 'node', startTime: 0, duration: 0, nodeStart: 0, v8Start: 0, bootstrapComplete: 0, environment: 0, loopStart: 0, loopExit: -1, idleTime: 0 },
};

if (typeof globalThis !== 'undefined' && !globalThis.performance) globalThis.performance = performance;

class PerformanceObserver {
  constructor(callback) { this._callback = callback; }
  observe() {}
  disconnect() {}
  takeRecords() { return []; }
}
PerformanceObserver.supportedEntryTypes = ['mark', 'measure', 'function', 'gc', 'http'];

function histogram() {
  return { enable() { return true; }, disable() { return true; }, reset() {}, record() {}, recordDelta() {},
    percentile() { return 0; }, percentiles: new Map(), min: 0, max: 0, mean: 0, stddev: 0, count: 0, exceeds: 0 };
}

module.exports = {
  performance,
  PerformanceObserver,
  PerformanceEntry,
  PerformanceMark,
  PerformanceMeasure,
  monitorEventLoopDelay: histogram,
  createHistogram: histogram,
  constants: {},
};
