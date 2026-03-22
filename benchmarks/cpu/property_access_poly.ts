const iterations = 10_000_000;
const obj1 = { a: 1 };
const obj2 = { b: 2, a: 2 };

function bench() {
    let sum = 0;
    for (let i = 0; i < iterations; i++) {
        const obj = (i & 1) ? obj1 : obj2;
        sum += obj.a;
    }
    return sum;
}

const start = Date.now();
const result = bench();
const end = Date.now();
console.log(`Polymorphic property access: ${end - start}ms (result=${result})`);
