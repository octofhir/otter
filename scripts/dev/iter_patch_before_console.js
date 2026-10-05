Array.prototype[Symbol.iterator] = function* () { yield 1; };
Map.prototype[Symbol.iterator] = function* () { yield [2, 2]; };
const s = new Set([1]);
console.log([...[5, 6]].join(), [...new Map([[3, 4]])].join(), s.size);
