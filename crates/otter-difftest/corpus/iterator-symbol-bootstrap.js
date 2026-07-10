const prototype = Iterator.prototype;
const iteratorMethod = prototype[Symbol.iterator];
const tagDescriptor = Object.getOwnPropertyDescriptor(prototype, Symbol.toStringTag);
const constructorDescriptor = Object.getOwnPropertyDescriptor(prototype, "constructor");

console.log(JSON.stringify({
  returnsReceiver: iteratorMethod.call(prototype) === prototype,
  tag: tagDescriptor.get.call(prototype),
  hasTagSetter: typeof tagDescriptor.set === "function",
  hasConstructorGetter: typeof constructorDescriptor.get === "function",
  hasConstructorSetter: typeof constructorDescriptor.set === "function",
}));
