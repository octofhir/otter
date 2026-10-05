const p = Object.getPrototypeOf([][Symbol.iterator]());
p.next = function () { return { done: true }; };
let n = 0;
for (const x of [1, 2, 3]) n++;
const [a] = [7];
console.log(n, a, [...[1, 2]].length);
