function bench() {
    const obj = { x: 1, y: 2, z: 3 };
    let sum = 0;
    const count = 1000000;

    // Warm up
    for (let i = 0; i < 1000; i++) {
        sum += obj.x + obj.y + obj.z;
    }

    const start = Date.now();
    for (let i = 0; i < count; i++) {
        sum += obj.x + obj.y + obj.z;
    }
    const end = Date.now();

    console.log("Property access benchmark (1M iterations):");
    console.log("Time: " + (end - start) + "ms");
    console.log("Sum: " + sum);
}

bench();
