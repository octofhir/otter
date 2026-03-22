/**
 * Frequent tiny-call workload focused on interpreter/JIT call overhead.
 *
 * Run with:
 *   otter run benchmarks/cpu/frequent_calls.ts [phase] [scale]
 *   node --experimental-strip-types benchmarks/cpu/frequent_calls.ts [phase] [scale]
 *   bun benchmarks/cpu/frequent_calls.ts [phase] [scale]
 *
 * Phases: all | simple-calls | percent-hex
 * Scale: integer multiplier for loop counts (default 1)
 */

const now = typeof performance !== "undefined"
    ? () => performance.now()
    : () => Date.now();

const phase = (process.argv[2] || "all").toLowerCase();
const scale = Math.max(1, Number.parseInt(process.argv[3] || "1", 10) || 1);

function runPhase(name: string, fn: () => number): void {
    const start = now();
    const result = fn();
    const duration = now() - start;
    console.log(`${name}: ${duration.toFixed(2)}ms (result=${result})`);
}

function simpleCallsPhase(mult: number): number {
    function add1(x: number): number {
        return x + 1;
    }

    let sum = 0;
    const iterations = 4_000_000 * mult;
    for (let i = 0; i < iterations; i++) {
        sum += add1(i & 1023);
    }
    return sum;
}

function decimalToPercentHexString(n: number): string {
    const hex = "0123456789ABCDEF";
    return "%" + hex[(n >> 4) & 0xf] + hex[n & 0xf];
}

function percentHexPhase(mult: number): number {
    let checksum = 0;
    const iterations = 983_040 * mult;

    for (let i = 0; i < iterations; i++) {
        const index = (i * 1103515245 + 12345) & 0x1fffff;

        const hex1 = decimalToPercentHexString(0x0080 + (index & 0x003f));
        const hex2 = decimalToPercentHexString(0x0080 + ((index & 0x0fc0) >> 6));
        const hex3 = decimalToPercentHexString(0x0080 + ((index & 0x3f000) >> 12));
        const hex4 = decimalToPercentHexString(0x00f0 + ((index & 0x1c0000) >> 18));

        checksum += hex1.charCodeAt(1) + hex1.charCodeAt(2);
        checksum += hex2.charCodeAt(1) + hex2.charCodeAt(2);
        checksum += hex3.charCodeAt(1) + hex3.charCodeAt(2);
        checksum += hex4.charCodeAt(1) + hex4.charCodeAt(2);
    }

    return checksum;
}

console.log("Frequent Tiny Call Workload");
console.log(`Phase: ${phase}, scale: ${scale}`);
console.log("=".repeat(50));

switch (phase) {
    case "simple-calls":
        runPhase("simple-calls", () => simpleCallsPhase(scale));
        break;
    case "percent-hex":
        runPhase("percent-hex", () => percentHexPhase(scale));
        break;
    case "all":
    default:
        runPhase("simple-calls", () => simpleCallsPhase(scale));
        runPhase("percent-hex", () => percentHexPhase(scale));
        break;
}

console.log("Done!");
