/**
 * Node.js perf_hooks module compatibility layer.
 *
 * Uses the browser Performance API where available.
 */

// Use the global performance object (Web standard, available in JSC)
const performance = globalThis.performance || {
  now: () => Date.now(),
  timeOrigin: Date.now(),
  mark: () => {},
  measure: () => {},
  getEntries: () => [],
  getEntriesByName: () => [],
  getEntriesByType: () => [],
  clearMarks: () => {},
  clearMeasures: () => {},
  clearResourceTimings: () => {},
  setResourceTimingBufferSize: () => {},
};

// PerformanceObserver stub
class PerformanceObserver {
  constructor(callback) {
    this._callback = callback;
  }

  observe(options) {
    // No-op in this stub
  }

  disconnect() {
    // No-op
  }

  takeRecords() {
    return [];
  }
}

// PerformanceEntry class
class PerformanceEntry {
  constructor(name, entryType, startTime, duration) {
    this.name = name;
    this.entryType = entryType;
    this.startTime = startTime;
    this.duration = duration;
  }

  toJSON() {
    return {
      name: this.name,
      entryType: this.entryType,
      startTime: this.startTime,
      duration: this.duration,
    };
  }
}

// monitorEventLoopDelay stub
function monitorEventLoopDelay(options) {
  const resolution = options?.resolution || 10;
  return {
    enable: () => {},
    disable: () => {},
    reset: () => {},
    percentile: (p) => 0,
    percentiles: new Map(),
    min: 0,
    max: 0,
    mean: 0,
    stddev: 0,
    exceeds: 0,
  };
}

// createHistogram stub
function createHistogram(options) {
  return {
    record: () => {},
    recordDelta: () => {},
    reset: () => {},
    percentile: (p) => 0,
    percentiles: new Map(),
    min: 0,
    max: 0,
    mean: 0,
    stddev: 0,
    count: 0,
    exceeds: 0,
  };
}

// Constants
const constants = {
  NODE_PERFORMANCE_GC_MAJOR: 1,
  NODE_PERFORMANCE_GC_MINOR: 2,
  NODE_PERFORMANCE_GC_INCREMENTAL: 4,
  NODE_PERFORMANCE_GC_WEAKCB: 8,
  NODE_PERFORMANCE_GC_FLAGS_NO: 0,
  NODE_PERFORMANCE_GC_FLAGS_CONSTRUCT_RETAINED: 1,
  NODE_PERFORMANCE_GC_FLAGS_FORCED: 2,
  NODE_PERFORMANCE_GC_FLAGS_SYNCHRONOUS_PHANTOM_PROCESSING: 4,
  NODE_PERFORMANCE_GC_FLAGS_ALL_AVAILABLE_GARBAGE: 8,
  NODE_PERFORMANCE_GC_FLAGS_ALL_EXTERNAL_MEMORY: 16,
  NODE_PERFORMANCE_GC_FLAGS_SCHEDULE_IDLE: 32,
};

const perfHooksModule = {
  performance,
  PerformanceObserver,
  PerformanceEntry,
  monitorEventLoopDelay,
  createHistogram,
  constants,
};

// Register as node:perf_hooks
if (typeof globalThis.__registerNodeBuiltin === 'function') {
  globalThis.__registerNodeBuiltin('perf_hooks', perfHooksModule);
}
