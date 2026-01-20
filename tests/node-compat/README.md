# Node.js Compatibility Test Suite

This directory contains infrastructure for running the official Node.js test suite against Otter.

## Quick Start

```bash
# Run all tests (fetches Node.js tests if needed)
./run.sh

# Or use just commands from the project root
just node-compat
```

## Directory Structure

```
tests/node-compat/
├── fetch-tests.sh          # Downloads Node.js test suite
├── run-node-tests.ts       # Main test runner
├── run.sh                  # Convenience script
├── check-regression.ts     # Regression detection
├── config/
│   ├── skip-list.json      # Tests to skip
│   ├── expected-failures.json  # Known failures
│   ├── module-status.json  # Per-module tracking
│   ├── test-filters.json   # Include/exclude patterns
│   └── timeout-overrides.json  # Custom timeouts
├── adapters/
│   ├── common.js           # Node.js test/common replacement
│   └── test-harness.js     # Test utilities
├── reports/
│   ├── latest.json         # Latest results (gitignored)
│   ├── baseline.json       # Committed baseline
│   └── history/            # Historical data (gitignored)
└── node-src/               # Downloaded Node.js tests (gitignored)
```

## Usage

### Run All Tests

```bash
./run.sh
# or
just node-compat
```

### Run Specific Module

```bash
./run.sh --module path
./run.sh --module buffer --verbose
# or
just node-compat-module path
```

### Run with Filter

```bash
otter run run-node-tests.ts --filter "test-path-join"
```

### Check for Regressions

```bash
otter run check-regression.ts
# or
just node-compat-check
```

### Update Baseline

After intentional changes that affect test results:

```bash
otter run check-regression.ts --update-baseline
# or
just node-compat-baseline
```

## Test Runner Options

```
Options:
  --module, -m <name>    Run tests for specific module only
  --filter, -f <pattern> Filter tests by regex pattern
  --parallel             Run only parallel tests
  --sequential           Run only sequential tests
  --verbose, -v          Show detailed output
  --json                 Output results as JSON
  --batch-size, -b <n>   Parallel batch size (default: 10)
  --timeout, -t <ms>     Default timeout (default: 30000)
```

## Configuration

### skip-list.json

Tests to skip, organized by reason:
- `patterns`: Prefix patterns (e.g., `test-inspector-`)
- `explicit`: Specific test files

### expected-failures.json

Known failures per module, updated as compatibility improves.

### timeout-overrides.json

Custom timeouts for slow tests (in milliseconds).

### test-filters.json

Global include/exclude regex patterns.

## Reports

### latest.json

Generated after each test run:
```json
{
  "timestamp": "2026-01-20T12:00:00.000Z",
  "summary": {
    "total": 500,
    "passed": 350,
    "failed": 100,
    "skipped": 50,
    "passRate": "77.8%"
  },
  "modules": {
    "path": { "total": 45, "passed": 45, "rate": "100%" },
    ...
  },
  "results": [...]
}
```

### baseline.json

Committed baseline for regression detection. Updated when intentional changes are made.

## CI Integration

The test suite runs automatically on:
- Push to main
- Pull requests
- Daily schedule

See `.github/workflows/node-compat.yml`.

## Adding New Module Support

1. Implement the module in `crates/otter-node/`
2. Run tests: `./run.sh --module <name> --verbose`
3. Add expected failures to `config/expected-failures.json`
4. Update baseline when ready

## Known Limitations

### CommonJS File Loading

Currently, Otter's `require()` implementation is optimized for bundled code and looks up
modules in internal registries rather than loading files from disk at runtime. This means:

- Built-in modules (`path`, `fs`, etc.) work correctly
- Bundled npm packages work correctly
- Loading arbitrary `.js` files via relative require does NOT work yet

**Impact:** Most Node.js tests use `require('../common')` to load test utilities. Until Otter
adds runtime file loading support, these tests will fail with "Cannot find module" errors.

**Workaround:** The test infrastructure is ready. Once Otter's runtime supports dynamic
CommonJS file loading, the tests will work automatically.

**Tracking:** This limitation should be addressed by enhancing `crates/otter-runtime/src/commonjs_runtime.js`
to support loading `.js` files from disk when they're not found in the module registry.

## Troubleshooting

### Tests not found

```bash
./fetch-tests.sh  # Re-download test suite
```

### Timeout issues

Increase timeout in `config/timeout-overrides.json` or use `--timeout` flag.

### Permission errors

Ensure running with all permissions:
```bash
otter run run-node-tests.ts --allow-read --allow-write --allow-net --allow-env --allow-run
```
