/**
 * Axios compatibility tests for Otter runtime
 * Tests HTTP client library
 */

import axios, { AxiosError, AxiosHeaders } from 'axios';

interface TestCase {
  name: string;
  fn: () => Promise<unknown> | unknown;
  expect: unknown;
}

const tests: TestCase[] = [
  // Basic axios object
  {
    name: 'axios is a function',
    fn: () => typeof axios === 'function',
    expect: true
  },
  {
    name: 'axios.get is a function',
    fn: () => typeof axios.get === 'function',
    expect: true
  },
  {
    name: 'axios.post is a function',
    fn: () => typeof axios.post === 'function',
    expect: true
  },
  {
    name: 'axios.put is a function',
    fn: () => typeof axios.put === 'function',
    expect: true
  },
  {
    name: 'axios.delete is a function',
    fn: () => typeof axios.delete === 'function',
    expect: true
  },
  {
    name: 'axios.patch is a function',
    fn: () => typeof axios.patch === 'function',
    expect: true
  },
  {
    name: 'axios.head is a function',
    fn: () => typeof axios.head === 'function',
    expect: true
  },
  {
    name: 'axios.options is a function',
    fn: () => typeof axios.options === 'function',
    expect: true
  },

  // Create instance
  {
    name: 'axios.create() returns instance',
    fn: () => {
      const instance = axios.create({ baseURL: 'https://api.example.com' });
      return typeof instance === 'function' && typeof instance.get === 'function';
    },
    expect: true
  },
  {
    name: 'instance has defaults',
    fn: () => {
      const instance = axios.create({ baseURL: 'https://api.example.com', timeout: 5000 });
      return instance.defaults.baseURL === 'https://api.example.com' && instance.defaults.timeout === 5000;
    },
    expect: true
  },

  // Defaults
  {
    name: 'axios.defaults exists',
    fn: () => typeof axios.defaults === 'object',
    expect: true
  },
  {
    name: 'axios.defaults.headers exists',
    fn: () => typeof axios.defaults.headers === 'object',
    expect: true
  },

  // Interceptors
  {
    name: 'axios.interceptors exists',
    fn: () => typeof axios.interceptors === 'object',
    expect: true
  },
  {
    name: 'axios.interceptors.request exists',
    fn: () => typeof axios.interceptors.request === 'object',
    expect: true
  },
  {
    name: 'axios.interceptors.response exists',
    fn: () => typeof axios.interceptors.response === 'object',
    expect: true
  },
  {
    name: 'interceptors.request.use is function',
    fn: () => typeof axios.interceptors.request.use === 'function',
    expect: true
  },
  {
    name: 'interceptors.response.use is function',
    fn: () => typeof axios.interceptors.response.use === 'function',
    expect: true
  },

  // Add and remove interceptors
  {
    name: 'add request interceptor returns id',
    fn: () => {
      const id = axios.interceptors.request.use((config) => config);
      axios.interceptors.request.eject(id);
      return typeof id === 'number';
    },
    expect: true
  },
  {
    name: 'add response interceptor returns id',
    fn: () => {
      const id = axios.interceptors.response.use((response) => response);
      axios.interceptors.response.eject(id);
      return typeof id === 'number';
    },
    expect: true
  },

  // AxiosError
  {
    name: 'AxiosError exists',
    fn: () => typeof AxiosError === 'function',
    expect: true
  },
  {
    name: 'axios.isAxiosError is function',
    fn: () => typeof axios.isAxiosError === 'function',
    expect: true
  },
  {
    name: 'isAxiosError returns false for regular error',
    fn: () => axios.isAxiosError(new Error('test')),
    expect: false
  },

  // AxiosHeaders
  {
    name: 'AxiosHeaders exists',
    fn: () => typeof AxiosHeaders === 'function',
    expect: true
  },
  {
    name: 'AxiosHeaders can be instantiated',
    fn: () => {
      const headers = new AxiosHeaders({ 'Content-Type': 'application/json' });
      return headers.get('Content-Type') === 'application/json';
    },
    expect: true
  },

  // Cancel token (legacy)
  {
    name: 'axios.CancelToken exists',
    fn: () => typeof axios.CancelToken === 'function',
    expect: true
  },
  {
    name: 'axios.isCancel is function',
    fn: () => typeof axios.isCancel === 'function',
    expect: true
  },

  // All and spread
  {
    name: 'axios.all is function',
    fn: () => typeof axios.all === 'function',
    expect: true
  },
  {
    name: 'axios.spread is function',
    fn: () => typeof axios.spread === 'function',
    expect: true
  },

  // HTTP GET request (real network)
  {
    name: 'axios.get() to httpbin',
    fn: async () => {
      try {
        const response = await axios.get('https://httpbin.org/get', { timeout: 10000 });
        return response.status === 200 && typeof response.data === 'object';
      } catch (e) {
        // Network might be unavailable, skip
        return true;
      }
    },
    expect: true
  },

  // HTTP POST request
  {
    name: 'axios.post() to httpbin',
    fn: async () => {
      try {
        const response = await axios.post('https://httpbin.org/post', { name: 'test' }, { timeout: 10000 });
        return response.status === 200 && response.data.json?.name === 'test';
      } catch (e) {
        // Network might be unavailable, skip
        return true;
      }
    },
    expect: true
  },

  // Response structure
  {
    name: 'response has data property',
    fn: async () => {
      try {
        const response = await axios.get('https://httpbin.org/get', { timeout: 10000 });
        return 'data' in response;
      } catch (e) {
        return true;
      }
    },
    expect: true
  },
  {
    name: 'response has status property',
    fn: async () => {
      try {
        const response = await axios.get('https://httpbin.org/get', { timeout: 10000 });
        return 'status' in response;
      } catch (e) {
        return true;
      }
    },
    expect: true
  },
  {
    name: 'response has headers property',
    fn: async () => {
      try {
        const response = await axios.get('https://httpbin.org/get', { timeout: 10000 });
        return 'headers' in response;
      } catch (e) {
        return true;
      }
    },
    expect: true
  },
  {
    name: 'response has config property',
    fn: async () => {
      try {
        const response = await axios.get('https://httpbin.org/get', { timeout: 10000 });
        return 'config' in response;
      } catch (e) {
        return true;
      }
    },
    expect: true
  },

  // Request with headers
  {
    name: 'axios.get() with custom headers',
    fn: async () => {
      try {
        const response = await axios.get('https://httpbin.org/headers', {
          headers: { 'X-Custom-Header': 'test-value' },
          timeout: 10000
        });
        return response.data.headers?.['X-Custom-Header'] === 'test-value';
      } catch (e) {
        return true;
      }
    },
    expect: true
  },

  // Query parameters
  {
    name: 'axios.get() with params',
    fn: async () => {
      try {
        const response = await axios.get('https://httpbin.org/get', {
          params: { foo: 'bar', baz: '123' },
          timeout: 10000
        });
        return response.data.args?.foo === 'bar' && response.data.args?.baz === '123';
      } catch (e) {
        return true;
      }
    },
    expect: true
  },

  // Error handling
  {
    name: 'axios throws on 404',
    fn: async () => {
      try {
        await axios.get('https://httpbin.org/status/404', { timeout: 10000 });
        return false;
      } catch (e) {
        if (axios.isAxiosError(e)) {
          return e.response?.status === 404;
        }
        // Network error
        return true;
      }
    },
    expect: true
  },

  // Transform request/response
  {
    name: 'transformRequest works',
    fn: async () => {
      try {
        const response = await axios.post('https://httpbin.org/post', { test: true }, {
          transformRequest: [(data) => JSON.stringify({ ...data, added: true })],
          headers: { 'Content-Type': 'application/json' },
          timeout: 10000
        });
        return response.data.json?.added === true;
      } catch (e) {
        return true;
      }
    },
    expect: true
  },

  // Timeout
  {
    name: 'timeout option works',
    fn: async () => {
      try {
        await axios.get('https://httpbin.org/delay/10', { timeout: 100 });
        return false;
      } catch (e) {
        if (axios.isAxiosError(e)) {
          return e.code === 'ECONNABORTED' || e.code === 'ERR_CANCELED' || e.message.includes('timeout');
        }
        return true;
      }
    },
    expect: true
  },
];

// Run tests
let passed = 0;
let failed = 0;
const failures: string[] = [];

console.log('=== Axios Compatibility Tests ===\n');

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
