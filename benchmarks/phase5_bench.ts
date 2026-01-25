const iterations = 5;
const count = 1000000;

function globalBench() {
    globalThis.a = 1;
    globalThis.b = 2;
    let total = 0;

    for (let j = 0; j < iterations; j++) {
        let sum = 0;
        const start = Date.now();
        for (let i = 0; i < count; i++) {
            sum += a + b;
        }
        total += (Date.now() - start);
    }
    console.log("Global access (avg of " + iterations + "M): " + (total / iterations).toFixed(2) + "ms");
}

function methodBench() {
    const obj = {
        x: 1,
        getY() { return 2; },
        getZ() { return 3; }
    };
    let total = 0;

    for (let j = 0; j < iterations; j++) {
        let sum = 0;
        const start = Date.now();
        for (let i = 0; i < count; i++) {
            sum += obj.x + obj.getY() + obj.getZ();
        }
        total += (Date.now() - start);
    }
    console.log("Method call (avg of " + iterations + "M): " + (total / iterations).toFixed(2) + "ms");
}

function computedMethodBench() {
    const obj = {
        x: 1,
        getY() { return 2; },
        getZ() { return 3; }
    };
    const keyY = "getY";
    const keyZ = "getZ";
    let total = 0;

    for (let j = 0; j < iterations; j++) {
        let sum = 0;
        const start = Date.now();
        for (let i = 0; i < count; i++) {
            sum += obj.x + obj[keyY]() + obj[keyZ]();
        }
        total += (Date.now() - start);
    }
    console.log("Computed method (avg of " + iterations + "M): " + (total / iterations).toFixed(2) + "ms");
}

console.log("--- Otter VM Phase 5 Performance (Release) ---");
globalBench();
methodBench();
computedMethodBench();
