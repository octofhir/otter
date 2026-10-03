// Cost of the three exact uncurrying forms a primordials table can use.
const ReflectApply = Reflect.apply;
const { bind, call } = Function.prototype;
const viaRest = (fn) => function (thisArg, ...args) { return ReflectApply(fn, thisArg, args); };
const viaBoundCall = bind.bind(call);
const forms = { rest: viaRest, bound: viaBoundCall };
const which = process.argv[2];
const push = forms[which](Array.prototype.push);
const slice = forms[which](String.prototype.slice);
const t0 = Date.now();
let n = 0;
for (let round = 0; round < 200000; round++) {
  const list = [];
  for (let i = 0; i < 10; i++) push(list, i);
  n += slice('abcdef', 1, 3).length + list.length;
}
console.log(which, n, Date.now() - t0, 'ms');
