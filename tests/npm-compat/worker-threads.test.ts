/**
 * worker_threads compatibility tests for Otter runtime
 * Tests the core node:worker_threads API surface and basic behavior
 */

import {
  Worker,
  isMainThread,
  parentPort,
  workerData,
  threadId,
  resourceLimits,
  SHARE_ENV,
  MessageChannel,
  MessagePort,
  BroadcastChannel,
  getEnvironmentData,
  setEnvironmentData,
  receiveMessageOnPort,
  markAsUntransferable,
  isMarkedAsUntransferable,
} from 'node:worker_threads';

interface TestCase {
  name: string;
  fn: () => unknown;
  expect: unknown;
}

const tests: TestCase[] = [
  // Module-level exports existence
  {
    name: 'worker_threads exports exist',
    fn: () => ({
      Worker: typeof Worker,
      isMainThread: typeof isMainThread,
      parentPort: typeof parentPort,
      workerData: typeof workerData,
      threadId: typeof threadId,
      resourceLimits: typeof resourceLimits,
      SHARE_ENV: typeof SHARE_ENV,
      MessageChannel: typeof MessageChannel,
      MessagePort: typeof MessagePort,
      BroadcastChannel: typeof BroadcastChannel,
      getEnvironmentData: typeof getEnvironmentData,
      setEnvironmentData: typeof setEnvironmentData,
      receiveMessageOnPort: typeof receiveMessageOnPort,
      markAsUntransferable: typeof markAsUntransferable,
      isMarkedAsUntransferable: typeof isMarkedAsUntransferable,
    }),
    expect: {
      Worker: 'function',
      isMainThread: 'boolean',
      parentPort: 'object', // null in main thread
      workerData: 'object', // null in main thread
      threadId: 'number',
      resourceLimits: 'object',
      SHARE_ENV: 'symbol',
      MessageChannel: 'function',
      MessagePort: 'function',
      BroadcastChannel: 'function',
      getEnvironmentData: 'function',
      setEnvironmentData: 'function',
      receiveMessageOnPort: 'function',
      markAsUntransferable: 'function',
      isMarkedAsUntransferable: 'function',
    },
  },
  // Main thread checks
  {
    name: 'isMainThread is true in main thread',
    fn: () => isMainThread,
    expect: true,
  },
  {
    name: 'parentPort is null in main thread',
    fn: () => parentPort,
    expect: null,
  },
  {
    name: 'workerData is null in main thread',
    fn: () => workerData,
    expect: null,
  },
  {
    name: 'threadId is a number (0 for main thread)',
    fn: () => threadId === 0,
    expect: true,
  },
  {
    name: 'SHARE_ENV is a symbol',
    fn: () => typeof SHARE_ENV,
    expect: 'symbol',
  },
  // MessageChannel tests
  {
    name: 'MessageChannel creates two linked ports',
    fn: () => {
      const ch = new MessageChannel();
      return {
        hasPort1: ch.port1 instanceof MessagePort,
        hasPort2: ch.port2 instanceof MessagePort,
        differentPorts: ch.port1 !== ch.port2,
      };
    },
    expect: {
      hasPort1: true,
      hasPort2: true,
      differentPorts: true,
    },
  },
  {
    name: 'MessagePort has required methods',
    fn: () => {
      const ch = new MessageChannel();
      return {
        postMessage: typeof ch.port1.postMessage,
        close: typeof ch.port1.close,
        start: typeof ch.port1.start,
        ref: typeof ch.port1.ref,
        unref: typeof ch.port1.unref,
        hasRef: typeof ch.port1.hasRef,
        on: typeof ch.port1.on,
      };
    },
    expect: {
      postMessage: 'function',
      close: 'function',
      start: 'function',
      ref: 'function',
      unref: 'function',
      hasRef: 'function',
      on: 'function',
    },
  },
  // NOTE: Async MessagePort tests require event loop integration
  // These are skipped for now as they would block
  {
    name: 'receiveMessageOnPort returns undefined when no message',
    fn: () => {
      const ch = new MessageChannel();
      const result = receiveMessageOnPort(ch.port2);
      ch.port1.close();
      ch.port2.close();
      return result === undefined;
    },
    expect: true,
  },
  // BroadcastChannel tests
  {
    name: 'BroadcastChannel constructor works',
    fn: () => {
      const bc = new BroadcastChannel('test-channel');
      const hasName = bc.name === 'test-channel';
      bc.close();
      return hasName;
    },
    expect: true,
  },
  {
    name: 'BroadcastChannel has required methods',
    fn: () => {
      const bc = new BroadcastChannel('test');
      const methods = {
        postMessage: typeof bc.postMessage,
        close: typeof bc.close,
        ref: typeof bc.ref,
        unref: typeof bc.unref,
      };
      bc.close();
      return methods;
    },
    expect: {
      postMessage: 'function',
      close: 'function',
      ref: 'function',
      unref: 'function',
    },
  },
  // NOTE: BroadcastChannel async communication requires event loop integration
  // Skipped for now as it would block
  // Environment data tests
  {
    name: 'setEnvironmentData and getEnvironmentData work',
    fn: () => {
      setEnvironmentData('testKey', { value: 42 });
      return getEnvironmentData('testKey');
    },
    expect: { value: 42 },
  },
  {
    name: 'getEnvironmentData returns null for unknown key',
    fn: () => getEnvironmentData('nonexistent'),
    expect: null,
  },
  // Untransferable tests
  {
    name: 'markAsUntransferable and isMarkedAsUntransferable work',
    fn: () => {
      const obj = { data: 'test' };
      markAsUntransferable(obj);
      return isMarkedAsUntransferable(obj);
    },
    expect: true,
  },
  {
    name: 'isMarkedAsUntransferable returns false for unmarked object',
    fn: () => {
      const obj = { data: 'test' };
      return isMarkedAsUntransferable(obj);
    },
    expect: false,
  },
  // Worker class tests
  {
    name: 'Worker class exists and is constructable',
    fn: () => {
      return typeof Worker === 'function';
    },
    expect: true,
  },
  {
    name: 'Worker has required methods',
    fn: () => {
      // Create a minimal worker (won't actually run, just checking methods)
      const worker = new Worker('console.log("test")', { eval: true });
      const methods = {
        postMessage: typeof worker.postMessage,
        terminate: typeof worker.terminate,
        ref: typeof worker.ref,
        unref: typeof worker.unref,
        on: typeof worker.on,
      };
      // Clean up
      worker.terminate();
      return methods;
    },
    expect: {
      postMessage: 'function',
      terminate: 'function',
      ref: 'function',
      unref: 'function',
      on: 'function',
    },
  },
  {
    name: 'Worker.threadId is a number',
    fn: () => {
      const worker = new Worker('', { eval: true });
      const isNumber = typeof worker.threadId === 'number';
      worker.terminate();
      return isNumber;
    },
    expect: true,
  },
  // NOTE: Worker async tests require event loop integration
  // These are synchronous tests for the Worker class structure
  {
    name: 'Worker.terminate returns a promise',
    fn: () => {
      const worker = new Worker('', { eval: true });
      const result = worker.terminate();
      // Don't await, just check it's a promise
      return result instanceof Promise;
    },
    expect: true,
  },
  // resourceLimits tests
  {
    name: 'resourceLimits is an object',
    fn: () => typeof resourceLimits === 'object' && resourceLimits !== null,
    expect: true,
  },
];

// Run tests
let passed = 0;
let failed = 0;
const failures: string[] = [];

console.log('=== worker_threads Compatibility Tests ===\n');

async function runTests() {
  for (const test of tests) {
    try {
      let result = test.fn();
      if (result instanceof Promise) {
        result = await result;
      }
      const resultStr = JSON.stringify(result);
      const expectStr = JSON.stringify(test.expect);

      if (resultStr === expectStr) {
        console.log(`PASS: ${test.name}`);
        passed++;
      } else {
        console.log(`FAIL: ${test.name}`);
        console.log(`  Expected: ${expectStr}`);
        console.log(`  Got: ${resultStr}`);
        failed++;
        failures.push(`${test.name}: expected ${expectStr}, got ${resultStr}`);
      }
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      console.log(`ERROR: ${test.name} - ${msg}`);
      failed++;
      failures.push(`${test.name}: ${msg}`);
    }
  }

  console.log('\n=== Summary ===');
  console.log(`Passed: ${passed}/${tests.length}`);
  console.log(`Failed: ${failed}/${tests.length}`);

  if (failures.length > 0) {
    console.log('\nFailures:');
    for (const f of failures) {
      console.log(`  - ${f}`);
    }
  }

  if (failed > 0) {
    process.exit(1);
  }
}

runTests();
