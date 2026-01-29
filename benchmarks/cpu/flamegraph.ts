/**
 * Flamegraph workload - mixes hot paths to reveal VM/runtime bottlenecks.
 *
 * Run with:
 *   otter run benchmarks/cpu/flamegraph.ts [phase] [scale]
 *   node --experimental-strip-types benchmarks/cpu/flamegraph.ts [phase] [scale]
 *   bun benchmarks/cpu/flamegraph.ts [phase] [scale]
 *
 * Phases: all | math | objects | arrays | strings | calls | json
 * Scale: integer multiplier for loop counts (default 1)
 */

// Polyfill for performance.now() if not available
const now = typeof performance !== "undefined"
    ? () => performance.now()
    : () => Date.now();

const phase = (process.argv[2] || "all").toLowerCase();
const scale = Math.max(1, Number.parseInt(process.argv[3] || "1", 10) || 1);

function runPhase(name: string, fn: () => number): void {
    const start = now();
    const result = fn();
    const duration = now() - start;
    // Print the result to keep the work observable (avoid dead code in other runtimes)
    console.log(`${name}: ${duration.toFixed(2)}ms (result=${result})`);
}

function mathPhase(mult: number): number {
    let acc = 0;
    const iterations = 5_000_000 * mult;
    for (let i = 0; i < iterations; i++) {
        acc = (acc + i) | 0;
        acc ^= (acc << 1);
        if ((acc & 1) === 0) {
            acc = (acc * 3 + 1) | 0;
        }
    }
    return acc;
}

function objectsPhase(mult: number): number {
    const count = 10_000 * mult;
    const objs: { a: number; b: number; c: number }[] = new Array(count);
    for (let i = 0; i < count; i++) {
        objs[i] = { a: i, b: i + 1, c: i + 2 };
    }

    let sum = 0;
    const loops = 200 * mult;
    for (let l = 0; l < loops; l++) {
        for (let i = 0; i < count; i++) {
            const obj = objs[i];
            sum += obj.a + obj.b;
            obj.b = obj.b + 1;
            obj.c = obj.b + obj.a;
        }
    }
    return sum;
}

function arraysPhase(mult: number): number {
    const size = 100_000 * mult;
    const arr = new Array<number>(size);
    for (let i = 0; i < size; i++) {
        arr[i] = i & 255;
    }

    let sum = 0;
    const loops = 50 * mult;
    for (let l = 0; l < loops; l++) {
        for (let i = 0; i < size; i++) {
            sum += arr[i];
        }
        for (let i = 0; i < 10_000; i++) {
            arr.push(i);
        }
        for (let i = 0; i < 10_000; i++) {
            arr.pop();
        }
    }
    return sum;
}

function stringsPhase(mult: number): number {
    let s = "";
    const iterations = 200_000 * mult;
    for (let i = 0; i < iterations; i++) {
        s += (i % 10).toString();
        if ((i & 1023) === 0) {
            s = s.slice(0, 4096) + "#" + s.slice(0, 4096);
        }
    }
    return s.length;
}

function callsPhase(mult: number): number {
    function add(a: number, b: number): number {
        return a + b;
    }

    function mul(a: number, b: number): number {
        return a * b;
    }

    function callChain(a: number, b: number): number {
        return mul(add(a, b), b - a);
    }

    let acc = 0;
    const iterations = 5_000_000 * mult;
    for (let i = 0; i < iterations; i++) {
        acc = callChain(acc, i);
    }
    return acc;
}

function jsonPhase(mult: number): number {
    const users = [] as Array<{ id: number; name: string; active: boolean }>;
    const count = 5_000 * mult;
    for (let i = 0; i < count; i++) {
        users.push({ id: i, name: `User ${i}`, active: (i & 1) === 0 });
    }
    const payload = JSON.stringify({ users, meta: { count, ts: Date.now() } });

    let sum = 0;
    const iterations = 500 * mult;
    for (let i = 0; i < iterations; i++) {
        const parsed = JSON.parse(payload);
        sum += parsed.meta.count + parsed.users.length;
        JSON.stringify(parsed);
    }
    return sum;
}

console.log("Flamegraph Workload");
console.log(`Phase: ${phase}, scale: ${scale}`);
console.log("=".repeat(50));

switch (phase) {
    case "math":
        runPhase("math", () => mathPhase(scale));
        break;
    case "objects":
        runPhase("objects", () => objectsPhase(scale));
        break;
    case "arrays":
        runPhase("arrays", () => arraysPhase(scale));
        break;
    case "strings":
        runPhase("strings", () => stringsPhase(scale));
        break;
    case "calls":
        runPhase("calls", () => callsPhase(scale));
        break;
    case "json":
        runPhase("json", () => jsonPhase(scale));
        break;
    case "all":
    default:
        runPhase("math", () => mathPhase(scale));
        runPhase("objects", () => objectsPhase(scale));
        runPhase("arrays", () => arraysPhase(scale));
        runPhase("strings", () => stringsPhase(scale));
        runPhase("calls", () => callsPhase(scale));
        runPhase("json", () => jsonPhase(scale));
        break;
}

console.log("Done!");
