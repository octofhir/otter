// Call continuations retain actuals, receiver, new.target and abrupt completion
// across ordinary, bound, constructor, native, proxy and suspended entries.
function recurse(n, box) {
  return n === 0 ? box.value : recurse(n - 1, box) + 1;
}
function actuals(a, b, ...rest) {
  return [a, b, arguments.length, rest.join(':')].join('/');
}
const bound = actuals.bind(null, 11).bind(null, 12);
let total = 0;
for (let i = 0; i < 512; i++) total += recurse(12, { value: 7 });

function Base(a, b, c) {
  this.values = [a, b, c];
  this.target = new.target;
}
const BoundBase = Base.bind(null, 21).bind(null, 22);
const base = new BoundBase(23);
class Derived extends Base {
  constructor(...args) { super(...args); this.kind = 'derived'; }
}
const derived = new Derived(31, 32, 33);

const receiver = { bias: 5 };
const proxyLog = [];
const proxy = new Proxy(function(a) { return this.bias + a; }, {
  get(target, key, receiver) { return Reflect.get(target, key, receiver); },
  apply(target, thisArg, args) {
    proxyLog.push(args.length);
    return Reflect.apply(target, thisArg, args);
  }
});
const proxyResult = proxy.call(receiver, 9);
const callback = [1, 2, 3].map(x => recurse(3, { value: x })).join(':');

let finallyCount = 0;
const thrown = { marker: 91 };
function throwing(n) {
  try {
    if (n === 0) throw thrown;
    return throwing(n - 1);
  } finally { finallyCount++; }
}
let caught;
try { throwing(8); } catch (error) { caught = error === thrown; }

function* suspended(a, ...rest) {
  try {
    yield arguments.length + ':' + rest.join('/');
    yield a;
  } catch (error) {
    yield error.marker + a;
  } finally { finallyCount++; }
}
const generator = suspended(4, 5, 6);
const first = generator.next();
const second = generator.throw({ marker: 70 });
const last = generator.return(88);
console.log(JSON.stringify([
  total, bound(13, 14), base.values, base.target === Base,
  derived.values, derived.target === Derived, derived.kind,
  proxyResult, proxyLog, callback, caught, finallyCount,
  first, second, last
]));

async function resumed(a, ...rest) {
  const box = { value: a };
  await null;
  return [recurse(2, box), arguments.length, rest.join(':')];
}
resumed(8, 9, 10).then(value => console.log(JSON.stringify(value)));
