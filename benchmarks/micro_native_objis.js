let s = 0;
const f = (i) => Object.is(i, 3) ? 1 : 2;
for (let i = 0; i < 1e8; i++) s += f(i);
console.log(s);
