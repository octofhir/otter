const iterations = 10_000_000;
const obj = { a: 1 };

function bench() {
    let sum = 0;
    for (let i = 0; i < iterations; i++) {
        sum += obj.a;
    }
    return sum;
}

const start = Date.now();
const result = bench();
const end = Date.now();
console.log(`Property access: ${end - start}ms (result=${result})`);
