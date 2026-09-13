// Receiver descriptor queries and proxy define preparation may both collect.
let descriptorCalls = 0;
let defineCalls = 0;
let proxyChecksum = 0;
function writeThroughProxy(target, receiver, value) {
  return Reflect.set(target, 'added', value, receiver);
}
for (let i = 0; i < 64; i++) {
  const target = {};
  const handler = i % 2 ? {
    getOwnPropertyDescriptor(target, key) {
      descriptorCalls++;
      const garbage = { first: {}, second: {}, third: {} };
      if (garbage.first === garbage.second) throw new Error('allocation identity');
      return Reflect.getOwnPropertyDescriptor(target, key);
    },
    defineProperty(target, key, descriptor) {
      defineCalls++;
      const garbage = { first: {}, second: {}, third: {} };
      if (garbage.first === garbage.second) throw new Error('allocation identity');
      return Reflect.defineProperty(target, key, descriptor);
    }
  } : {};
  const receiver = new Proxy(target, handler);
  const payload = { marker: i };
  if (!writeThroughProxy(target, receiver, payload) || target.added !== payload) {
    throw new Error('proxy receiver lost stored identity');
  }
  if (target.added.marker !== i) throw new Error('proxy receiver lost payload');
  proxyChecksum += target.added.marker;
}
JSON.stringify([64, descriptorCalls, defineCalls, proxyChecksum]);
