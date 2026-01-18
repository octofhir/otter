
function run() {
    return new Promise<number>((resolve) => {
        const start = performance.now();
        let ticks = 0;
        const duration = 1000; // Run for 1s

        function tick() {
            ticks++;
            if (performance.now() - start < duration) {
                // Schedule next tick immediately
                queueMicrotask(tick);
                // Also mix in some setImmediate/setTimeout(0) if available to stress macro-task queue too?
                // For now, strict microtask flooding to test starvation limit.
            } else {
                const end = performance.now();
                // Ops/sec = ticks / (seconds)
                resolve(ticks / ((end - start) / 1000));
            }
        }

        tick();
    });
}

// If running directly
run().then((ops) => {
    console.log(JSON.stringify({ name: "tick-throughput", ops }));
}).catch(e => {
    console.error(e);
});
