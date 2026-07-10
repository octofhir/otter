function* values() {
  yield 1;
  return 2;
}

const iterator = values();
const shared = Object.getPrototypeOf(Object.getPrototypeOf(iterator));
const constructor = shared.constructor;

console.log(JSON.stringify({
  first: iterator.next(),
  second: iterator.next(),
  methods: ["next", "return", "throw"].map((name) => typeof shared[name]),
  constructorTag: constructor[Symbol.toStringTag],
  prototypeLink: constructor.prototype === shared,
}));
