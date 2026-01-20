/**
 * Node.js Compatibility Test Runner for Otter
 *
 * Runs official Node.js test suite against Otter runtime
 * and generates compatibility reports.
 *
 * Usage:
 *   otter run run-node-tests.ts [options]
 *
 * Options:
 *   --module, -m <name>    Run tests for specific module only
 *   --filter, -f <pattern> Filter tests by regex pattern
 *   --parallel             Run only parallel tests
 *   --sequential           Run only sequential tests
 *   --verbose, -v          Show detailed output
 *   --json                 Output results as JSON
 *   --batch-size, -b <n>   Number of parallel tests per batch (default: 10)
 *   --timeout, -t <ms>     Default timeout per test (default: 30000)
 *   --help, -h             Show this help message
 */

import { spawn } from 'child_process';
import * as fs from 'fs';
import * as path from 'path';

// =============================================================================
// Types
// =============================================================================

interface TestResult {
  file: string;
  module: string;
  status: 'passed' | 'failed' | 'skipped' | 'timeout' | 'error';
  duration: number;
  error?: string;
  stderr?: string;
  exitCode?: number;
}

interface ModuleStats {
  total: number;
  passed: number;
  failed: number;
  skipped: number;
  rate: string;
}

interface TestReport {
  timestamp: string;
  otterVersion: string;
  nodeVersion: string;
  platform: string;
  summary: {
    total: number;
    passed: number;
    failed: number;
    skipped: number;
    passRate: string;
  };
  modules: Record<string, ModuleStats>;
  results: TestResult[];
  duration: number;
}

interface SkipConfig {
  patterns: Record<string, string[]>;
  explicit: string[];
}

interface Config {
  skipList: SkipConfig;
  expectedFailures: Record<string, string[]>;
  timeoutOverrides: Record<string, number>;
  testFilters: {
    include: string[];
    exclude: string[];
  };
}

interface CLIOptions {
  module?: string;
  filter?: string;
  parallel: boolean;
  sequential: boolean;
  verbose: boolean;
  json: boolean;
  batchSize: number;
  timeout: number;
  help: boolean;
}

// =============================================================================
// Constants
// =============================================================================

const SCRIPT_DIR = import.meta.dirname!;
const CONFIG_DIR = path.join(SCRIPT_DIR, 'config');
const REPORTS_DIR = path.join(SCRIPT_DIR, 'reports');
const TEST_DIR = path.join(SCRIPT_DIR, 'node-src', 'test');

const DEFAULT_TIMEOUT = 30000;
const DEFAULT_BATCH_SIZE = 10;

// =============================================================================
// Configuration Loading
// =============================================================================

function loadConfig(): Config {
  try {
    const skipList: SkipConfig = JSON.parse(
      fs.readFileSync(path.join(CONFIG_DIR, 'skip-list.json'), 'utf-8')
    );
    const expectedFailures: Record<string, string[]> = JSON.parse(
      fs.readFileSync(path.join(CONFIG_DIR, 'expected-failures.json'), 'utf-8')
    );
    const timeoutOverrides: Record<string, number> = JSON.parse(
      fs.readFileSync(path.join(CONFIG_DIR, 'timeout-overrides.json'), 'utf-8')
    );
    const testFilters = JSON.parse(
      fs.readFileSync(path.join(CONFIG_DIR, 'test-filters.json'), 'utf-8')
    );

    return { skipList, expectedFailures, timeoutOverrides, testFilters };
  } catch (error) {
    console.error('Error loading config:', error);
    process.exit(1);
  }
}

// =============================================================================
// Test Discovery
// =============================================================================

function shouldSkipFile(filename: string, config: Config): boolean {
  // Check explicit skip list
  if (config.skipList.explicit.includes(filename)) {
    return true;
  }

  // Check pattern-based skip list
  for (const patterns of Object.values(config.skipList.patterns)) {
    for (const pattern of patterns) {
      if (filename.startsWith(pattern) || filename.includes(pattern)) {
        return true;
      }
    }
  }

  return false;
}

function matchesFilters(file: string, config: Config): boolean {
  const filters = config.testFilters;

  // Check include patterns
  if (filters.include && filters.include.length > 0) {
    const included = filters.include.some((p) => new RegExp(p).test(file));
    if (!included) return false;
  }

  // Check exclude patterns
  if (filters.exclude && filters.exclude.length > 0) {
    const excluded = filters.exclude.some((p) => new RegExp(p).test(file));
    if (excluded) return false;
  }

  return true;
}

function discoverTests(config: Config, options: CLIOptions): string[] {
  const tests: string[] = [];
  const dirs: string[] = [];

  if (!options.sequential) {
    dirs.push('parallel');
  }
  if (!options.parallel) {
    dirs.push('sequential');
  }

  for (const subdir of dirs) {
    const dirPath = path.join(TEST_DIR, subdir);
    if (!fs.existsSync(dirPath)) {
      continue;
    }

    try {
      const files = fs.readdirSync(dirPath);
      const testFiles = files
        .filter((f) => f.startsWith('test-') && f.endsWith('.js'))
        .filter((f) => !shouldSkipFile(f, config))
        .filter((f) => matchesFilters(f, config))
        .map((f) => path.join(subdir, f));

      tests.push(...testFiles);
    } catch (error) {
      console.error(`Error reading directory ${dirPath}:`, error);
    }
  }

  return tests;
}

// =============================================================================
// Module Extraction
// =============================================================================

function extractModule(file: string): string {
  const basename = path.basename(file);

  // test-path-*.js -> path
  // test-buffer-*.js -> buffer
  // test-child-process-*.js -> child-process
  // test-async-hooks-*.js -> async-hooks
  const match = basename.match(/^test-([a-z]+(?:-[a-z]+)*)/);

  if (match) {
    const rawModule = match[1];

    // Map common variations
    const moduleMap: Record<string, string> = {
      'child': 'child-process',
      'async': 'async-hooks',
      'worker': 'worker-threads',
      'string': 'string-decoder',
      'perf': 'perf-hooks',
    };

    // Check if this is a known module prefix
    for (const [prefix, mapped] of Object.entries(moduleMap)) {
      if (rawModule === prefix || rawModule.startsWith(`${prefix}-`)) {
        return mapped;
      }
    }

    return rawModule;
  }

  return 'other';
}

// =============================================================================
// Test Execution
// =============================================================================

async function runTest(
  file: string,
  config: Config,
  options: CLIOptions
): Promise<TestResult> {
  const fullPath = path.join(TEST_DIR, file);
  const module = extractModule(file);
  const basename = path.basename(file);
  const timeout = config.timeoutOverrides[basename] || options.timeout;
  const startTime = Date.now();

  // Use the otter binary from the project's target/release directory
  // This ensures we use the freshly built version, not the one in PATH
  // SCRIPT_DIR is tests/node-compat, so go up twice to project root
  // Use path.resolve to ensure the path is absolute and doesn't contain '..'
  const otterBinary = path.resolve(SCRIPT_DIR, '..', '..', 'target', 'release', 'otter');


  // Use shell timeout because Otter's setTimeout doesn't work with child processes
  const timeoutSec = Math.ceil(timeout / 1000);

  const spawnArgs = [
    String(timeoutSec),
    otterBinary,
    'run',
    fullPath,
    '--allow-read',
    '--allow-write',
    '--allow-net',
    '--allow-env',
    '--allow-run',
    '--timeout',
    String(timeoutSec),
  ];

  return new Promise((resolve) => {
    const proc = spawn(
      'timeout',
      spawnArgs,
      {
        cwd: SCRIPT_DIR,
        stdio: ['pipe', 'pipe', 'pipe'],
        // Note: Don't pass custom env as it triggers env_clear() in Rust
        // which breaks the timeout command. The env vars we wanted to set
        // are not essential for the tests anyway.
      }
    );

    let stdout = '';
    let stderr = '';

    // Note: We use shell `timeout` command instead of JS setTimeout
    // because Otter's event loop doesn't process timers while child processes run
    // Exit code 124 from `timeout` means the process was killed

    proc.stdout?.on('data', (data) => {
      stdout += data.toString();
    });

    proc.stderr?.on('data', (data) => {
      stderr += data.toString();
    });

    proc.on('close', (code) => {
      const duration = Date.now() - startTime;

      // Check for skip indication
      if (stdout.includes('# Skipped:') || stdout.includes('1..0')) {
        resolve({
          file,
          module,
          status: 'skipped',
          duration,
        });
        return;
      }

      // Exit code 124 = killed by timeout command
      if (code === 124) {
        resolve({
          file,
          module,
          status: 'timeout',
          duration,
          error: `Test timed out after ${timeout}ms`,
        });
      } else if (code === 0) {
        resolve({
          file,
          module,
          status: 'passed',
          duration,
        });
      } else {
        // Get a concise error message
        let error = stderr || stdout;
        const lines = error.split('\n').filter((l) => l.trim());

        // Try to find the actual error
        const errorLine = lines.find(
          (l) =>
            l.includes('Error:') ||
            l.includes('AssertionError') ||
            l.includes('FAIL')
        );

        if (errorLine) {
          error = errorLine.slice(0, 200);
        } else if (lines.length > 0) {
          error = lines.slice(-3).join('\n').slice(0, 300);
        }

        resolve({
          file,
          module,
          status: 'failed',
          duration,
          error,
          exitCode: code ?? undefined,
        });
      }
    });

    proc.on('error', (err) => {
      clearTimeout(timer);
      resolve({
        file,
        module,
        status: 'error',
        duration: Date.now() - startTime,
        error: err.message,
      });
    });
  });
}

// =============================================================================
// Report Generation
// =============================================================================

function generateReport(results: TestResult[], startTime: number): TestReport {
  const modules: Record<string, ModuleStats> = {};

  for (const result of results) {
    if (!modules[result.module]) {
      modules[result.module] = {
        total: 0,
        passed: 0,
        failed: 0,
        skipped: 0,
        rate: '0%',
      };
    }

    modules[result.module].total++;
    if (result.status === 'passed') {
      modules[result.module].passed++;
    } else if (result.status === 'skipped') {
      modules[result.module].skipped++;
    } else {
      modules[result.module].failed++;
    }
  }

  // Calculate rates
  for (const mod of Object.values(modules)) {
    const effective = mod.total - mod.skipped;
    if (effective > 0) {
      mod.rate = ((mod.passed / effective) * 100).toFixed(1) + '%';
    } else {
      mod.rate = 'N/A';
    }
  }

  const passed = results.filter((r) => r.status === 'passed').length;
  const failed = results.filter(
    (r) => r.status !== 'passed' && r.status !== 'skipped'
  ).length;
  const skipped = results.filter((r) => r.status === 'skipped').length;
  const effective = results.length - skipped;

  return {
    timestamp: new Date().toISOString(),
    otterVersion: process.env.OTTER_VERSION || 'unknown',
    nodeVersion: 'v24.x',
    platform: process.platform,
    summary: {
      total: results.length,
      passed,
      failed,
      skipped,
      passRate: effective > 0 ? ((passed / effective) * 100).toFixed(1) + '%' : '0%',
    },
    modules,
    results,
    duration: Date.now() - startTime,
  };
}

// =============================================================================
// CLI Parsing
// =============================================================================

function parseArgs(): CLIOptions {
  const args = process.argv.slice(2);
  const options: CLIOptions = {
    parallel: false,
    sequential: false,
    verbose: false,
    json: false,
    batchSize: DEFAULT_BATCH_SIZE,
    timeout: DEFAULT_TIMEOUT,
    help: false,
  };

  for (let i = 0; i < args.length; i++) {
    switch (args[i]) {
      case '--module':
      case '-m':
        options.module = args[++i];
        break;
      case '--filter':
      case '-f':
        options.filter = args[++i];
        break;
      case '--parallel':
        options.parallel = true;
        break;
      case '--sequential':
        options.sequential = true;
        break;
      case '--verbose':
      case '-v':
        options.verbose = true;
        break;
      case '--json':
        options.json = true;
        break;
      case '--batch-size':
      case '-b':
        options.batchSize = parseInt(args[++i], 10) || DEFAULT_BATCH_SIZE;
        break;
      case '--timeout':
      case '-t':
        options.timeout = parseInt(args[++i], 10) || DEFAULT_TIMEOUT;
        break;
      case '--help':
      case '-h':
        options.help = true;
        break;
    }
  }

  return options;
}

function showHelp(): void {
  console.log(`
Node.js Compatibility Test Runner for Otter

Usage:
  otter run run-node-tests.ts [options]

Options:
  --module, -m <name>    Run tests for specific module only (e.g., path, buffer)
  --filter, -f <pattern> Filter tests by regex pattern
  --parallel             Run only parallel tests
  --sequential           Run only sequential tests
  --verbose, -v          Show detailed output for each test
  --json                 Output results as JSON
  --batch-size, -b <n>   Number of parallel tests per batch (default: 10)
  --timeout, -t <ms>     Default timeout per test in ms (default: 30000)
  --help, -h             Show this help message

Examples:
  otter run run-node-tests.ts                    # Run all tests
  otter run run-node-tests.ts --module path      # Run only path module tests
  otter run run-node-tests.ts --filter "test-path-join" --verbose
  otter run run-node-tests.ts --json > results.json
`);
}

// =============================================================================
// Progress Display
// =============================================================================

function printProgress(
  current: number,
  total: number,
  passed: number,
  failed: number
): void {
  const percent = ((current / total) * 100).toFixed(0);
  const bar = '='.repeat(Math.floor((current / total) * 30));
  const spaces = ' '.repeat(30 - bar.length);
  process.stdout.write(
    `\r[${bar}${spaces}] ${percent}% (${current}/${total}) | Passed: ${passed} | Failed: ${failed}`
  );
}

// =============================================================================
// Main
// =============================================================================

async function main(): Promise<void> {
  const options = parseArgs();

  if (options.help) {
    showHelp();
    process.exit(0);
  }

  // Check if node-src exists
  if (!fs.existsSync(TEST_DIR)) {
    console.error('Error: Node.js test suite not found.');
    console.error('Run ./fetch-tests.sh first to download the test suite.');
    process.exit(1);
  }

  const config = loadConfig();
  const startTime = Date.now();

  // Discover tests
  let tests = discoverTests(config, options);

  // Apply CLI filters
  if (options.module) {
    tests = tests.filter((t) => extractModule(t) === options.module);
  }
  if (options.filter) {
    const regex = new RegExp(options.filter);
    tests = tests.filter((t) => regex.test(t));
  }

  if (tests.length === 0) {
    console.error('No tests found matching the criteria.');
    process.exit(1);
  }

  if (!options.json) {
    console.log('='.repeat(60));
    console.log('Otter Node.js Compatibility Test Suite');
    console.log('='.repeat(60));
    console.log(`Tests discovered: ${tests.length}`);
    console.log(`Batch size: ${options.batchSize}`);
    console.log(`Timeout: ${options.timeout}ms`);
    console.log('');
  }

  // Run tests
  const results: TestResult[] = [];
  let passed = 0;
  let failed = 0;

  // Split into parallel and sequential
  const parallelTests = tests.filter((t) => t.startsWith('parallel/'));
  const sequentialTests = tests.filter((t) => t.startsWith('sequential/'));

  // Run parallel tests in batches
  if (parallelTests.length > 0 && !options.json) {
    console.log(`Running ${parallelTests.length} parallel tests...`);
  }

  for (let i = 0; i < parallelTests.length; i += options.batchSize) {
    const batch = parallelTests.slice(i, i + options.batchSize);
    console.log(`DEBUG: Starting batch ${i / options.batchSize + 1}, tests ${i}-${i + batch.length}`);
    const batchResults = await Promise.all(
      batch.map((test) => runTest(test, config, options))
    );
    console.log(`DEBUG: Batch completed with ${batchResults.length} results`);

    for (const result of batchResults) {
      results.push(result);

      if (result.status === 'passed') passed++;
      else if (result.status !== 'skipped') failed++;

      if (options.verbose && !options.json) {
        const icon =
          result.status === 'passed'
            ? 'PASS'
            : result.status === 'skipped'
              ? 'SKIP'
              : 'FAIL';
        console.log(`${icon}: ${result.file} (${result.duration}ms)`);
        if (result.error && result.status === 'failed') {
          console.log(`      Error: ${result.error.slice(0, 100)}`);
        }
      }
    }

    if (!options.json && !options.verbose) {
      const completed = Math.min(i + batch.length, parallelTests.length);
      if (completed % 10 === 0 || completed === parallelTests.length) {
        printProgress(
          completed,
          parallelTests.length,
          passed,
          failed
        );
      }
    }
  }

  if (!options.json && parallelTests.length > 0 && !options.verbose) {
    console.log(''); // New line after progress bar
  }

  // Run sequential tests one at a time
  if (sequentialTests.length > 0 && !options.json) {
    console.log(`\nRunning ${sequentialTests.length} sequential tests...`);
  }

  for (let i = 0; i < sequentialTests.length; i++) {
    const result = await runTest(sequentialTests[i], config, options);
    results.push(result);

    if (result.status === 'passed') passed++;
    else if (result.status !== 'skipped') failed++;

    if (options.verbose && !options.json) {
      const icon =
        result.status === 'passed'
          ? 'PASS'
          : result.status === 'skipped'
            ? 'SKIP'
            : 'FAIL';
      console.log(`${icon}: ${result.file} (${result.duration}ms)`);
      if (result.error && result.status === 'failed') {
        console.log(`      Error: ${result.error.slice(0, 100)}`);
      }
    } else if (!options.json) {
      printProgress(i + 1, sequentialTests.length, passed, failed);
    }
  }

  if (!options.json && sequentialTests.length > 0 && !options.verbose) {
    console.log(''); // New line after progress bar
  }

  // Generate report
  const report = generateReport(results, startTime);

  // Ensure reports directory exists
  fs.mkdirSync(REPORTS_DIR, { recursive: true });
  fs.mkdirSync(path.join(REPORTS_DIR, 'history'), { recursive: true });

  // Save reports
  const latestPath = path.join(REPORTS_DIR, 'latest.json');
  const historyPath = path.join(
    REPORTS_DIR,
    'history',
    `${new Date().toISOString().replace(/[:.]/g, '-')}.json`
  );

  fs.writeFileSync(latestPath, JSON.stringify(report, null, 2));
  fs.writeFileSync(historyPath, JSON.stringify(report, null, 2));

  // Output
  if (options.json) {
    console.log(JSON.stringify(report, null, 2));
  } else {
    console.log('\n' + '='.repeat(60));
    console.log('SUMMARY');
    console.log('='.repeat(60));
    console.log(`Total:     ${report.summary.total}`);
    console.log(`Passed:    ${report.summary.passed}`);
    console.log(`Failed:    ${report.summary.failed}`);
    console.log(`Skipped:   ${report.summary.skipped}`);
    console.log(`Pass Rate: ${report.summary.passRate}`);
    console.log(`Duration:  ${(report.duration / 1000).toFixed(1)}s`);
    console.log('');

    // Per-module breakdown
    console.log('Per-Module Results:');
    const sortedModules = Object.entries(report.modules).sort(
      ([a], [b]) => a.localeCompare(b)
    );

    for (const [mod, stats] of sortedModules) {
      if (stats.total > 0) {
        const status =
          stats.failed === 0
            ? 'ok'
            : stats.passed === 0
              ? 'bad'
              : 'partial';
        const icon = status === 'ok' ? '' : status === 'partial' ? '' : '';
        console.log(
          `  ${mod}: ${stats.passed}/${stats.total - stats.skipped} (${stats.rate}) ${icon}`
        );
      }
    }

    console.log('');
    console.log(`Report saved: ${latestPath}`);

    // Show top failures if not verbose
    if (!options.verbose && report.summary.failed > 0) {
      console.log('\nTop failures:');
      const failures = results
        .filter((r) => r.status === 'failed')
        .slice(0, 5);
      for (const f of failures) {
        console.log(`  - ${f.file}`);
        if (f.error) {
          console.log(`    ${f.error.slice(0, 80)}...`);
        }
      }
      if (report.summary.failed > 5) {
        console.log(`  ... and ${report.summary.failed - 5} more`);
      }
    }
  }

  // Exit with failure if tests failed
  if (report.summary.failed > 0) {
    process.exit(1);
  }
}

main().catch((err) => {
  console.error('Fatal error:', err?.message || err?.toString?.() || JSON.stringify(err) || err);
  if (err?.stack) {
    console.error('Stack:', err.stack);
  }
  process.exit(1);
});
