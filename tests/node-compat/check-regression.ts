/**
 * Regression Checker for Node.js Compatibility Tests
 *
 * Compares current test results with baseline to detect regressions.
 *
 * Usage:
 *   otter run check-regression.ts [options]
 *
 * Options:
 *   --update-baseline    Update baseline with current results
 *   --json               Output as JSON
 *   --strict             Fail on any regression (default)
 *   --warn-only          Only warn on regressions, don't fail
 *   --help, -h           Show help
 */

import * as fs from 'fs';
import * as path from 'path';

// =============================================================================
// Types
// =============================================================================

interface ModuleStats {
  total: number;
  passed: number;
  failed: number;
  skipped: number;
  rate: string;
}

interface TestResult {
  file: string;
  module: string;
  status: 'passed' | 'failed' | 'skipped' | 'timeout' | 'error';
}

interface TestReport {
  timestamp: string;
  summary: {
    total: number;
    passed: number;
    failed: number;
    skipped: number;
    passRate: string;
  };
  modules: Record<string, ModuleStats>;
  results: TestResult[];
}

interface RegressionReport {
  timestamp: string;
  regressions: string[];
  improvements: string[];
  moduleChanges: Array<{
    module: string;
    before: string;
    after: string;
    change: string;
  }>;
  overallChange: {
    before: string;
    after: string;
    change: string;
  };
  hasRegressions: boolean;
}

interface CLIOptions {
  updateBaseline: boolean;
  json: boolean;
  strict: boolean;
  help: boolean;
}

// =============================================================================
// Constants
// =============================================================================

const SCRIPT_DIR = import.meta.dirname!;
const REPORTS_DIR = path.join(SCRIPT_DIR, 'reports');
const LATEST_PATH = path.join(REPORTS_DIR, 'latest.json');
const BASELINE_PATH = path.join(REPORTS_DIR, 'baseline.json');

// =============================================================================
// CLI Parsing
// =============================================================================

function parseArgs(): CLIOptions {
  const args = process.argv.slice(2);
  const options: CLIOptions = {
    updateBaseline: false,
    json: false,
    strict: true,
    help: false,
  };

  for (const arg of args) {
    switch (arg) {
      case '--update-baseline':
        options.updateBaseline = true;
        break;
      case '--json':
        options.json = true;
        break;
      case '--strict':
        options.strict = true;
        break;
      case '--warn-only':
        options.strict = false;
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
Regression Checker for Node.js Compatibility Tests

Usage:
  otter run check-regression.ts [options]

Options:
  --update-baseline    Update baseline with current results
  --json               Output as JSON
  --strict             Fail on any regression (default)
  --warn-only          Only warn on regressions, don't fail
  --help, -h           Show this help message

Examples:
  otter run check-regression.ts              # Check for regressions
  otter run check-regression.ts --json       # Output as JSON
  otter run check-regression.ts --update-baseline  # Update baseline
`);
}

// =============================================================================
// Regression Analysis
// =============================================================================

function analyzeRegressions(
  latest: TestReport,
  baseline: TestReport
): RegressionReport {
  // Find regressions (tests that passed before but fail now)
  const baselinePassed = new Set(
    baseline.results.filter((r) => r.status === 'passed').map((r) => r.file)
  );
  const latestFailed = latest.results.filter(
    (r) => r.status !== 'passed' && r.status !== 'skipped'
  );
  const regressions = latestFailed
    .filter((r) => baselinePassed.has(r.file))
    .map((r) => r.file);

  // Find improvements (tests that failed before but pass now)
  const baselineFailed = new Set(
    baseline.results
      .filter((r) => r.status !== 'passed' && r.status !== 'skipped')
      .map((r) => r.file)
  );
  const latestPassed = latest.results.filter((r) => r.status === 'passed');
  const improvements = latestPassed
    .filter((r) => baselineFailed.has(r.file))
    .map((r) => r.file);

  // Module changes
  const moduleChanges: RegressionReport['moduleChanges'] = [];
  const allModules = new Set([
    ...Object.keys(latest.modules),
    ...Object.keys(baseline.modules),
  ]);

  for (const mod of allModules) {
    const latestStats = latest.modules[mod];
    const baselineStats = baseline.modules[mod];

    if (!latestStats || !baselineStats) {
      if (latestStats) {
        moduleChanges.push({
          module: mod,
          before: 'N/A',
          after: latestStats.rate,
          change: 'NEW',
        });
      }
      continue;
    }

    const latestRate = parseFloat(latestStats.rate) || 0;
    const baselineRate = parseFloat(baselineStats.rate) || 0;
    const diff = latestRate - baselineRate;

    if (Math.abs(diff) > 0.1) {
      moduleChanges.push({
        module: mod,
        before: baselineStats.rate,
        after: latestStats.rate,
        change: (diff >= 0 ? '+' : '') + diff.toFixed(1) + '%',
      });
    }
  }

  // Overall change
  const latestRate = parseFloat(latest.summary.passRate) || 0;
  const baselineRate = parseFloat(baseline.summary.passRate) || 0;
  const overallDiff = latestRate - baselineRate;

  return {
    timestamp: new Date().toISOString(),
    regressions,
    improvements,
    moduleChanges,
    overallChange: {
      before: baseline.summary.passRate,
      after: latest.summary.passRate,
      change: (overallDiff >= 0 ? '+' : '') + overallDiff.toFixed(1) + '%',
    },
    hasRegressions: regressions.length > 0,
  };
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

  // Check if latest report exists
  if (!fs.existsSync(LATEST_PATH)) {
    console.error('Error: No latest.json report found.');
    console.error('Run tests first: otter run run-node-tests.ts');
    process.exit(1);
  }

  const latest: TestReport = JSON.parse(fs.readFileSync(LATEST_PATH, 'utf-8'));

  // Update baseline if requested
  if (options.updateBaseline) {
    fs.copyFileSync(LATEST_PATH, BASELINE_PATH);
    console.log('Baseline updated from latest results.');
    process.exit(0);
  }

  // Check if baseline exists
  if (!fs.existsSync(BASELINE_PATH)) {
    if (!options.json) {
      console.log('No baseline.json found. Creating from latest results.');
    }
    fs.copyFileSync(LATEST_PATH, BASELINE_PATH);
    if (!options.json) {
      console.log('Baseline created. No regression check performed.');
    }
    process.exit(0);
  }

  const baseline: TestReport = JSON.parse(
    fs.readFileSync(BASELINE_PATH, 'utf-8')
  );

  // Analyze regressions
  const report = analyzeRegressions(latest, baseline);

  // Output
  if (options.json) {
    console.log(JSON.stringify(report, null, 2));
  } else {
    console.log('='.repeat(60));
    console.log('Regression Check');
    console.log('='.repeat(60));
    console.log('');

    // Overall change
    console.log(
      `Pass Rate: ${report.overallChange.after} (baseline: ${report.overallChange.before})`
    );
    console.log(`Change:    ${report.overallChange.change}`);
    console.log('');

    // Regressions
    if (report.regressions.length > 0) {
      console.log(`REGRESSIONS: ${report.regressions.length} tests`);
      console.log('-'.repeat(40));
      for (const file of report.regressions.slice(0, 20)) {
        console.log(`  - ${file}`);
      }
      if (report.regressions.length > 20) {
        console.log(`  ... and ${report.regressions.length - 20} more`);
      }
      console.log('');
    }

    // Improvements
    if (report.improvements.length > 0) {
      console.log(`IMPROVEMENTS: ${report.improvements.length} tests now passing`);
      console.log('-'.repeat(40));
      for (const file of report.improvements.slice(0, 10)) {
        console.log(`  + ${file}`);
      }
      if (report.improvements.length > 10) {
        console.log(`  ... and ${report.improvements.length - 10} more`);
      }
      console.log('');
    }

    // Module changes
    if (report.moduleChanges.length > 0) {
      console.log('Module Changes:');
      console.log('-'.repeat(40));
      for (const change of report.moduleChanges.sort((a, b) =>
        a.module.localeCompare(b.module)
      )) {
        const arrow = change.change.startsWith('+') ? '+' : '';
        console.log(`  ${change.module}: ${change.after} (${change.change})`);
      }
      console.log('');
    }

    // Final status
    console.log('='.repeat(60));
    if (report.hasRegressions) {
      console.log('RESULT: REGRESSIONS DETECTED');
    } else if (report.improvements.length > 0) {
      console.log('RESULT: IMPROVEMENTS (no regressions)');
    } else {
      console.log('RESULT: NO CHANGES');
    }
    console.log('='.repeat(60));
  }

  // Exit with failure if regressions detected and strict mode
  if (report.hasRegressions && options.strict) {
    process.exit(1);
  }
}

main().catch((err) => {
  console.error('Fatal error:', err);
  process.exit(1);
});
