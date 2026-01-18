
async function run() {
    const iterations = 50;
    const targetDelay = 10;

    // Warmup
    await new Promise(r => setTimeout(r, 10));

    const delays: number[] = [];

    for (let i = 0; i < iterations; i++) {
        const start = performance.now();
        await new Promise(r => setTimeout(r, targetDelay));
        const end = performance.now();
        delays.push(end - start - targetDelay);
    }

    // Calculate average drift and p99
    delays.sort((a, b) => a - b);
    const avg = delays.reduce((a, b) => a + b, 0) / delays.length;
    const p99 = delays[Math.floor(delays.length * 0.99)];

    console.log(`Timer drift (target ${targetDelay}ms): Avg=${avg.toFixed(3)}ms, P99=${p99.toFixed(3)}ms`);
    return avg; // Lower is better
}

run();
