const {
  constants,
  gzipSync,
  gunzipSync,
  deflateSync,
  inflateSync,
  deflateRawSync,
  inflateRawSync,
  brotliCompressSync,
  brotliDecompressSync,
  crc32,
} = require('node:zlib');

const tests = [
  {
    name: 'can round-trip gzip default options',
    fn: () => {
      const data = Buffer.from('hello zlib');
      const compressed = gzipSync(data);
      const decompressed = gunzipSync(compressed);
      return decompressed.toString();
    },
    expect: 'hello zlib',
  },
  {
    name: 'zlib deflate honors level option',
    fn: () => {
      const opts = { level: constants.Z_BEST_COMPRESSION };
      const data = Buffer.from('compress me lots');
      const compressed = deflateSync(data, opts);
      const decompressed = inflateSync(compressed, opts);
      return decompressed.toString();
    },
    expect: 'compress me lots',
  },
  {
    name: 'raw deflate/inflate works',
    fn: () => {
      const data = Buffer.from('bare deflate');
      const compressed = deflateRawSync(data);
      const decompressed = inflateRawSync(compressed);
      return decompressed.toString();
    },
    expect: 'bare deflate',
  },
  {
    name: 'brotli round-trips with quality param',
    fn: () => {
      const opts = { params: { BROTLI_PARAM_QUALITY: 5 } };
      const data = Buffer.from('brotli test');
      const compressed = brotliCompressSync(data, opts);
      const decompressed = brotliDecompressSync(compressed);
      return decompressed.toString();
    },
    expect: 'brotli test',
  },
  {
    name: 'crc32 matches expected',
    fn: () => crc32('crc'),
    expect: 0x765E7680,
  },
];

let passed = 0;
let failed = 0;
const failures = [];

console.log('=== zlib Compatibility Tests ===\n');

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
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
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
    for (const failure of failures) {
      console.log(`  - ${failure}`);
    }
  }

  if (failed > 0) {
    process.exit(1);
  }
}

runTests();
