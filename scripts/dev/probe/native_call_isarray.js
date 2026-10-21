// A long loop of one cheap native builtin call, for sampling its call path.
const arr = [1, 2, 3];
let c = 0;
for (let i = 0; i < 1e8; i++) if (Array.isArray(arr)) c++;
console.log(c);
