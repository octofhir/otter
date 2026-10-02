function hasFlag(v, f) { return (v & f) != 0; }
function work(n) { let c = 0; for (let i = 0; i < n; i++) { if (hasFlag(i, 4)) c++; } return c; }
let t = 0;
for (let r = 0; r < 50; r++) t += work(2000);
console.log(t);
