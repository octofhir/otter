const arr = Array.from({ length: 1000 }, (_, i) => i);
let sum = 0;
function cb(x) { sum += x; }
const t = Date.now();
for (let i = 0; i < 40000; i++) arr.forEach(cb);
console.log(Date.now() - t, "ms", sum);
