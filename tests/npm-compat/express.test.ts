/**
 * Express compatibility tests for Otter runtime
 * Tests the core express module imports and basic functionality
 */

import express, { Request, Response, NextFunction } from 'express';

interface TestCase {
  name: string;
  fn: () => unknown;
  expect: unknown;
}

const tests: TestCase[] = [
  // Basic imports
  { name: 'express is a function', fn: () => typeof express, expect: 'function' },
  { name: 'express.Router exists', fn: () => typeof express.Router, expect: 'function' },
  { name: 'express.json exists', fn: () => typeof express.json, expect: 'function' },
  { name: 'express.urlencoded exists', fn: () => typeof express.urlencoded, expect: 'function' },
  { name: 'express.static exists', fn: () => typeof express.static, expect: 'function' },
  { name: 'express.raw exists', fn: () => typeof express.raw, expect: 'function' },
  { name: 'express.text exists', fn: () => typeof express.text, expect: 'function' },

  // Creating app
  {
    name: 'express() creates app',
    fn: () => {
      const app = express();
      return typeof app;
    },
    expect: 'function'
  },
  {
    name: 'app has get method',
    fn: () => {
      const app = express();
      return typeof app.get;
    },
    expect: 'function'
  },
  {
    name: 'app has post method',
    fn: () => {
      const app = express();
      return typeof app.post;
    },
    expect: 'function'
  },
  {
    name: 'app has put method',
    fn: () => {
      const app = express();
      return typeof app.put;
    },
    expect: 'function'
  },
  {
    name: 'app has delete method',
    fn: () => {
      const app = express();
      return typeof app.delete;
    },
    expect: 'function'
  },
  {
    name: 'app has patch method',
    fn: () => {
      const app = express();
      return typeof app.patch;
    },
    expect: 'function'
  },
  {
    name: 'app has use method',
    fn: () => {
      const app = express();
      return typeof app.use;
    },
    expect: 'function'
  },
  {
    name: 'app has listen method',
    fn: () => {
      const app = express();
      return typeof app.listen;
    },
    expect: 'function'
  },
  {
    name: 'app has set method',
    fn: () => {
      const app = express();
      return typeof app.set;
    },
    expect: 'function'
  },
  {
    name: 'app has enable method',
    fn: () => {
      const app = express();
      return typeof app.enable;
    },
    expect: 'function'
  },
  {
    name: 'app has disable method',
    fn: () => {
      const app = express();
      return typeof app.disable;
    },
    expect: 'function'
  },
  {
    name: 'app has enabled method',
    fn: () => {
      const app = express();
      return typeof app.enabled;
    },
    expect: 'function'
  },
  {
    name: 'app has disabled method',
    fn: () => {
      const app = express();
      return typeof app.disabled;
    },
    expect: 'function'
  },
  {
    name: 'app has route method',
    fn: () => {
      const app = express();
      return typeof app.route;
    },
    expect: 'function'
  },
  {
    name: 'app has all method',
    fn: () => {
      const app = express();
      return typeof app.all;
    },
    expect: 'function'
  },
  {
    name: 'app has engine method',
    fn: () => {
      const app = express();
      return typeof app.engine;
    },
    expect: 'function'
  },
  {
    name: 'app has param method',
    fn: () => {
      const app = express();
      return typeof app.param;
    },
    expect: 'function'
  },
  {
    name: 'app has path method',
    fn: () => {
      const app = express();
      return typeof app.path;
    },
    expect: 'function'
  },
  {
    name: 'app has render method',
    fn: () => {
      const app = express();
      return typeof app.render;
    },
    expect: 'function'
  },

  // Router
  {
    name: 'Router() creates router',
    fn: () => {
      const router = express.Router();
      return typeof router;
    },
    expect: 'function'
  },
  {
    name: 'router has get method',
    fn: () => {
      const router = express.Router();
      return typeof router.get;
    },
    expect: 'function'
  },
  {
    name: 'router has post method',
    fn: () => {
      const router = express.Router();
      return typeof router.post;
    },
    expect: 'function'
  },
  {
    name: 'router has use method',
    fn: () => {
      const router = express.Router();
      return typeof router.use;
    },
    expect: 'function'
  },
  {
    name: 'router has route method',
    fn: () => {
      const router = express.Router();
      return typeof router.route;
    },
    expect: 'function'
  },
  {
    name: 'router has param method',
    fn: () => {
      const router = express.Router();
      return typeof router.param;
    },
    expect: 'function'
  },

  // App settings
  {
    name: 'app.set/get works',
    fn: () => {
      const app = express();
      app.set('foo', 'bar');
      return app.get('foo');
    },
    expect: 'bar'
  },
  {
    name: 'app.enable/enabled works',
    fn: () => {
      const app = express();
      app.enable('trust proxy');
      return app.enabled('trust proxy');
    },
    expect: true
  },
  {
    name: 'app.disable/disabled works',
    fn: () => {
      const app = express();
      app.disable('x-powered-by');
      return app.disabled('x-powered-by');
    },
    expect: true
  },

  // Route registration (no actual request handling)
  {
    name: 'app.get registers route',
    fn: () => {
      const app = express();
      const result = app.get('/', (req: Request, res: Response) => res.send('hello'));
      return result === app; // Returns app for chaining
    },
    expect: true
  },
  {
    name: 'app.post registers route',
    fn: () => {
      const app = express();
      const result = app.post('/data', (req: Request, res: Response) => res.json({}));
      return result === app;
    },
    expect: true
  },
  {
    name: 'app.use registers middleware',
    fn: () => {
      const app = express();
      const result = app.use((req: Request, res: Response, next: NextFunction) => next());
      return result === app;
    },
    expect: true
  },
  {
    name: 'app.use with path registers middleware',
    fn: () => {
      const app = express();
      const result = app.use('/api', (req: Request, res: Response, next: NextFunction) => next());
      return result === app;
    },
    expect: true
  },
  {
    name: 'app.route creates route',
    fn: () => {
      const app = express();
      const route = app.route('/users');
      return typeof route.get === 'function' && typeof route.post === 'function';
    },
    expect: true
  },

  // Middleware
  {
    name: 'express.json() returns function',
    fn: () => typeof express.json(),
    expect: 'function'
  },
  {
    name: 'express.urlencoded() returns function',
    fn: () => typeof express.urlencoded({ extended: true }),
    expect: 'function'
  },
  {
    name: 'express.raw() returns function',
    fn: () => typeof express.raw(),
    expect: 'function'
  },
  {
    name: 'express.text() returns function',
    fn: () => typeof express.text(),
    expect: 'function'
  },

  // Application locals
  {
    name: 'app.locals exists',
    fn: () => {
      const app = express();
      return typeof app.locals;
    },
    expect: 'object'
  },
  {
    name: 'app.locals is writable',
    fn: () => {
      const app = express();
      app.locals.title = 'My App';
      return app.locals.title;
    },
    expect: 'My App'
  },

  // Mounting
  {
    name: 'app.mountpath default',
    fn: () => {
      const app = express();
      return app.mountpath;
    },
    expect: '/'
  },

  // Request application properties (mock test)
  {
    name: 'app.request exists',
    fn: () => {
      const app = express();
      return typeof app.request;
    },
    expect: 'object'
  },
  {
    name: 'app.response exists',
    fn: () => {
      const app = express();
      return typeof app.response;
    },
    expect: 'object'
  },

  // Router chaining
  {
    name: 'router methods are chainable',
    fn: () => {
      const router = express.Router();
      const result = router
        .get('/', (req: Request, res: Response) => res.send('GET'))
        .post('/', (req: Request, res: Response) => res.send('POST'))
        .put('/', (req: Request, res: Response) => res.send('PUT'))
        .delete('/', (req: Request, res: Response) => res.send('DELETE'));
      return result === router;
    },
    expect: true
  },

  // Router with options
  {
    name: 'Router with caseSensitive option',
    fn: () => {
      const router = express.Router({ caseSensitive: true });
      return typeof router;
    },
    expect: 'function'
  },
  {
    name: 'Router with strict option',
    fn: () => {
      const router = express.Router({ strict: true });
      return typeof router;
    },
    expect: 'function'
  },
  {
    name: 'Router with mergeParams option',
    fn: () => {
      const router = express.Router({ mergeParams: true });
      return typeof router;
    },
    expect: 'function'
  },

  // Multiple handlers
  {
    name: 'app.get accepts multiple handlers',
    fn: () => {
      const app = express();
      const handler1 = (req: Request, res: Response, next: NextFunction) => next();
      const handler2 = (req: Request, res: Response) => res.send('done');
      const result = app.get('/multi', handler1, handler2);
      return result === app;
    },
    expect: true
  },
  {
    name: 'app.get accepts handler array',
    fn: () => {
      const app = express();
      const handlers = [
        (req: Request, res: Response, next: NextFunction) => next(),
        (req: Request, res: Response) => res.send('done')
      ];
      const result = app.get('/array', handlers);
      return result === app;
    },
    expect: true
  },
];

// Run tests
let passed = 0;
let failed = 0;
const failures: string[] = [];

console.log('=== Express Compatibility Tests ===\n');

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
