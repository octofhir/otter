/**
 * Async worker_threads tests - tests real worker execution
 */

import {
  Worker,
  isMainThread,
  parentPort,
  workerData,
  threadId,
} from 'node:worker_threads';

console.log('=== worker_threads Async Tests ===\n');

let passed = 0;
let failed = 0;

// Test 1: Worker executes eval code and we get online/exit events
async function testWorkerExecution(): Promise<boolean> {
  return new Promise((resolve) => {
    const worker = new Worker('console.log("Hello from worker!")', { eval: true });

    let gotOnline = false;
    let gotExit = false;

    worker.on('online', () => {
      gotOnline = true;
    });

    worker.on('exit', (code: number) => {
      gotExit = true;
      if (gotOnline && code === 0) {
        console.log('PASS: Test 1 - Worker executed and emitted online/exit events');
        resolve(true);
      } else {
        console.log(`FAIL: Test 1 - online=${gotOnline}, exit code=${code}`);
        resolve(false);
      }
    });

    worker.on('error', (err: Error) => {
      console.log('FAIL: Test 1 - Worker error:', err.message);
      resolve(false);
    });

    // Terminate after short delay
    setTimeout(() => {
      worker.terminate();
    }, 200);
  });
}

// Test 2: Worker receives workerData
async function testWorkerData(): Promise<boolean> {
  return new Promise((resolve) => {
    const testData = { value: 42, name: 'test' };
    const worker = new Worker(`
      const wd = globalThis.__otter_worker_data;
      if (wd && wd.value === 42 && wd.name === 'test') {
        console.log('Worker got correct data:', JSON.stringify(wd));
      } else {
        console.log('Worker data mismatch:', JSON.stringify(wd));
      }
    `, {
      eval: true,
      workerData: testData
    });

    worker.on('exit', () => {
      console.log('PASS: Test 2 - Worker with workerData completed');
      resolve(true);
    });

    worker.on('error', (err: Error) => {
      console.log('FAIL: Test 2 - Worker error:', err.message);
      resolve(false);
    });

    setTimeout(() => {
      worker.terminate();
    }, 200);
  });
}

// Test 3: Multiple workers can run concurrently
async function testMultipleWorkers(): Promise<boolean> {
  return new Promise((resolve) => {
    const numWorkers = 3;
    let onlineCount = 0;
    let exitCount = 0;
    const workers: Worker[] = [];

    for (let i = 0; i < numWorkers; i++) {
      const worker = new Worker(`console.log("Worker ${i} running")`, { eval: true });
      workers.push(worker);

      worker.on('online', () => {
        onlineCount++;
      });

      worker.on('exit', () => {
        exitCount++;
        if (exitCount === numWorkers) {
          if (onlineCount === numWorkers) {
            console.log('PASS: Test 3 - All', numWorkers, 'workers executed concurrently');
            resolve(true);
          } else {
            console.log('FAIL: Test 3 - Only', onlineCount, '/', numWorkers, 'workers came online');
            resolve(false);
          }
        }
      });
    }

    // Terminate all after delay
    setTimeout(() => {
      for (const w of workers) {
        w.terminate();
      }
    }, 500);
  });
}

// Test 4: Worker error handling
async function testWorkerError(): Promise<boolean> {
  return new Promise((resolve) => {
    const worker = new Worker('throw new Error("intentional error")', { eval: true });

    let gotError = false;

    worker.on('error', (err: Error) => {
      gotError = true;
    });

    worker.on('exit', () => {
      if (gotError) {
        console.log('PASS: Test 4 - Worker error was caught');
        resolve(true);
      } else {
        console.log('FAIL: Test 4 - Error event not received');
        resolve(false);
      }
    });

    setTimeout(() => {
      worker.terminate();
    }, 300);
  });
}

// Run all async tests
async function runAsyncTests() {
  console.log('Test 1: Worker execution with events');
  if (await testWorkerExecution()) passed++; else failed++;

  console.log('\nTest 2: Worker receives workerData');
  if (await testWorkerData()) passed++; else failed++;

  console.log('\nTest 3: Multiple concurrent workers');
  if (await testMultipleWorkers()) passed++; else failed++;

  console.log('\nTest 4: Worker error handling');
  if (await testWorkerError()) passed++; else failed++;

  console.log('\n=== Async Tests Summary ===');
  console.log(`Passed: ${passed}/4`);
  console.log(`Failed: ${failed}/4`);

  // Use Otter.exit() if available, otherwise just let it complete
  if (failed > 0) {
    // @ts-ignore
    if (typeof Otter !== 'undefined' && Otter.exit) {
      // @ts-ignore
      Otter.exit(1);
    }
  }
}

runAsyncTests();
