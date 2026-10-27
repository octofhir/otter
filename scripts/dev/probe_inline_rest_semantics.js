// Rest-parameter bodies inlined into hot callers: list identity, mutation
// before apply, deopt inside, and the boolean arithmetic fold.
const out = [];
function sum(...xs) { let s = 0; for (const x of xs) s += x; return s; }
function first(a, ...rest) { return rest.length ? rest[0] : a; }
function mutateThenApply(f, ...args) { args[0] = 100; return Reflect.apply(f, null, args); }
function pushThenApply(f, ...args) { args.push(7); return Reflect.apply(f, null, args); }
function keep(...args) { return args; }
const add = (a, b = 0, c = 0) => a + b + c;
let acc = 0;
for (let i = 0; i < 50000; i++) {
  acc += sum(i, 1, 2) + first(i) + first(i, 3) + mutateThenApply(add, i, 1) + pushThenApply(add, 1, 2);
  acc += (i & 1) === 0;
  acc -= keep(i).length;
}
out.push(acc, sum(), sum(1.5, 'x'), first(9), keep(1, 2).length, Array.isArray(keep()));
const k = keep(1, 2); k.push(3); out.push(k.join(), keep(1) !== keep(1));
out.push(sum(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18));
let c = 0; for (let i = 0; i < 1000; i++) { c += i > 500; c += true * 2; } out.push(c);
console.log(JSON.stringify(out));
