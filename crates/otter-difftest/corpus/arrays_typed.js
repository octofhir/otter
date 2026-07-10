const dense = [1, 2, 3];
const holey = [1, , 3];
const typed = new Int32Array(8);
for (let i = 0; i < 100; i++) { dense.push(i); typed[i & 7] += i; }
JSON.stringify({ dense: dense.length, hole: 1 in holey, typed: Array.from(typed) });
