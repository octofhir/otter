# Otter Benchmark Suite

Performance benchmarks comparing Otter with Node.js and Bun.

## Quick Start

```bash
# Run all benchmarks
./benchmarks/bench.sh all

# Run specific category
./benchmarks/bench.sh startup
./benchmarks/bench.sh cpu
./benchmarks/bench.sh memory

# Run specific benchmark
./benchmarks/bench.sh cpu/fibonacci.ts
```

## Running Manually

### Otter
```bash
otter run benchmarks/cpu/fibonacci.ts
```

### Node.js (requires v22+ for TypeScript)
```bash
node --experimental-strip-types benchmarks/cpu/fibonacci.ts
```

### Bun
```bash
bun benchmarks/cpu/fibonacci.ts
```

## Benchmark Categories

### Startup (`startup/`)
- `hello-world.ts` - Minimal script execution time

### CPU (`cpu/`)
- `fibonacci.ts` - Recursive and iterative computation
- `json.ts` - JSON parsing and serialization

### Memory (`memory/`)
- `allocation.ts` - Object and array allocation patterns

## Metrics

| Metric | Description |
|--------|-------------|
| Real time | Wall-clock time |
| User time | CPU time in user mode |
| Sys time | CPU time in kernel mode |
| Max RSS | Maximum resident set size |

## Results Format

Results are stored in `benchmarks/results/` as JSON:

```json
{
  "name": "fibonacci-recursive",
  "category": "cpu",
  "runtime": "otter",
  "version": "v0.1.0",
  "metrics": {
    "duration_ms": 1234.56,
    "ops_per_sec": 8100
  },
  "timestamp": "2026-01-16T12:00:00.000Z"
}
```

## Adding New Benchmarks

1. Create a new `.ts` file in the appropriate category folder
2. Include a docstring with run instructions
3. Use `performance.now()` for timing
4. Output results to console

Example:
```typescript
function benchmark(name: string, fn: () => void, iterations: number) {
    const start = performance.now();
    for (let i = 0; i < iterations; i++) {
        fn();
    }
    const duration = performance.now() - start;
    console.log(`${name}: ${duration.toFixed(2)}ms`);
}
```
