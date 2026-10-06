function* g(n) { for (let i = 0; i < n; i++) yield i; }
let s = 0;
const N = +process.argv[2];
for (let k = 0; k < N; k++) for (const x of g(4)) s += x;
console.log(s);
