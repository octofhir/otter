
async function run() {
    const iterations = 100_000;
    const start = performance.now();

    const promises = [];
    for (let i = 0; i < iterations; i++) {
        promises.push(Promise.resolve(i));
    }

    await Promise.all(promises);

    const end = performance.now();
    const durationS = (end - start) / 1000;
    const ops = iterations / durationS;

    console.log(`Promise throughput: ${Math.floor(ops)} ops/sec`);
    return ops;
}

run();
