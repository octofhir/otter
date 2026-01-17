/**
 * k6 HTTP Load Test Script
 *
 * Tests HTTP server performance with various scenarios.
 *
 * Usage:
 *   k6 run benchmarks/http/load-test.js --env URL=http://localhost:3000
 *   k6 run benchmarks/http/load-test.js --env URL=http://localhost:3001 --env NAME=otter
 */

import http from 'k6/http';
import { check, sleep } from 'k6';
import { Rate, Trend } from 'k6/metrics';

// Custom metrics
const errorRate = new Rate('errors');
const responseTime = new Trend('response_time', true);

// Configuration
const BASE_URL = __ENV.URL || 'http://localhost:3000';
const RUNTIME_NAME = __ENV.NAME || 'unknown';

export const options = {
    scenarios: {
        // Smoke test - basic functionality
        smoke: {
            executor: 'constant-vus',
            vus: 1,
            duration: '5s',
            tags: { scenario: 'smoke' },
            startTime: '0s',
        },
        // Load test - sustained traffic
        load: {
            executor: 'constant-vus',
            vus: 50,
            duration: '30s',
            tags: { scenario: 'load' },
            startTime: '10s',
        },
        // Stress test - increasing load
        stress: {
            executor: 'ramping-vus',
            startVUs: 0,
            stages: [
                { duration: '10s', target: 100 },
                { duration: '20s', target: 100 },
                { duration: '10s', target: 200 },
                { duration: '20s', target: 200 },
                { duration: '10s', target: 0 },
            ],
            tags: { scenario: 'stress' },
            startTime: '50s',
        },
    },
    thresholds: {
        http_req_duration: ['p(95)<100', 'p(99)<200'],
        errors: ['rate<0.01'],
    },
    summaryTrendStats: ['avg', 'min', 'med', 'max', 'p(90)', 'p(95)', 'p(99)'],
};

// Test scenarios
export default function () {
    // Hello World endpoint
    const helloRes = http.get(`${BASE_URL}/`);
    check(helloRes, {
        'hello status 200': (r) => r.status === 200,
        'hello body correct': (r) => r.body === 'Hello, World!',
    });
    errorRate.add(helloRes.status !== 200);
    responseTime.add(helloRes.timings.duration);

    // JSON endpoint
    const jsonRes = http.get(`${BASE_URL}/json`);
    check(jsonRes, {
        'json status 200': (r) => r.status === 200,
        'json content-type': (r) => r.headers['Content-Type']?.includes('application/json'),
    });
    errorRate.add(jsonRes.status !== 200);
    responseTime.add(jsonRes.timings.duration);

    // Small delay to avoid overwhelming
    sleep(0.01);
}

// Summary handler for custom output
export function handleSummary(data) {
    const summary = {
        runtime: RUNTIME_NAME,
        url: BASE_URL,
        timestamp: new Date().toISOString(),
        metrics: {
            requests_total: data.metrics.http_reqs?.values?.count || 0,
            requests_per_second: data.metrics.http_reqs?.values?.rate || 0,
            response_time_avg: data.metrics.http_req_duration?.values?.avg || 0,
            response_time_p95: data.metrics.http_req_duration?.values?.['p(95)'] || 0,
            response_time_p99: data.metrics.http_req_duration?.values?.['p(99)'] || 0,
            error_rate: data.metrics.errors?.values?.rate || 0,
        },
    };

    return {
        stdout: textSummary(data, { indent: '  ', enableColors: true }),
        [`benchmarks/results/http-${RUNTIME_NAME}-${Date.now()}.json`]: JSON.stringify(summary, null, 2),
    };
}

// Simple text summary (k6 doesn't export it by default in newer versions)
function textSummary(data, opts = {}) {
    const { indent = '', enableColors = false } = opts;
    const c = enableColors ? {
        green: '\x1b[32m',
        red: '\x1b[31m',
        cyan: '\x1b[36m',
        reset: '\x1b[0m'
    } : { green: '', red: '', cyan: '', reset: '' };

    let output = `\n${c.cyan}=== HTTP Server Benchmark Results ===${c.reset}\n\n`;
    output += `${indent}Runtime: ${RUNTIME_NAME}\n`;
    output += `${indent}URL: ${BASE_URL}\n\n`;

    const reqs = data.metrics.http_reqs;
    const duration = data.metrics.http_req_duration;

    if (reqs && reqs.values) {
        output += `${c.green}Requests:${c.reset}\n`;
        output += `${indent}  Total: ${reqs.values.count}\n`;
        output += `${indent}  Rate: ${reqs.values.rate?.toFixed(2)} req/s\n\n`;
    }

    if (duration && duration.values) {
        output += `${c.green}Response Time:${c.reset}\n`;
        output += `${indent}  Avg: ${duration.values.avg?.toFixed(2)}ms\n`;
        output += `${indent}  Min: ${duration.values.min?.toFixed(2)}ms\n`;
        output += `${indent}  Max: ${duration.values.max?.toFixed(2)}ms\n`;
        output += `${indent}  P90: ${duration.values['p(90)']?.toFixed(2)}ms\n`;
        output += `${indent}  P95: ${duration.values['p(95)']?.toFixed(2)}ms\n`;
        output += `${indent}  P99: ${duration.values['p(99)']?.toFixed(2)}ms\n\n`;
    }

    return output;
}
