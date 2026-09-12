// Hot store-IC guards must preserve prototype, descriptor and exotic effects.
const proto = { x: 0 };
function put(receiver, value) {
  'use strict';
  receiver.x = value;
  return value;
}
let checksum = 0;
for (let i = 0; i < 6000; i++) checksum += put(Object.create(proto), i);

Object.defineProperty(proto, 'x', { writable: false });
const blocked = Object.create(proto);
let readonly = false;
try { put(blocked, 6); } catch (error) { readonly = error instanceof TypeError; }
readonly = readonly && !Object.hasOwn(blocked, 'x');

let setters = 0;
let last = 0;
Object.defineProperty(proto, 'x', {
  configurable: true,
  set(value) { setters++; last = value; this.side = value + 1; }
});
const observed = Object.create(proto);
put(observed, 7);

const own = Object.create(proto);
Object.defineProperty(own, 'x', { value: 2, writable: true });
put(own, 9);

delete proto.x;
const sealed = Object.preventExtensions(Object.create(proto));
let nonextensible = false;
try { put(sealed, 11); } catch (error) { nonextensible = error instanceof TypeError; }
nonextensible = nonextensible && !Object.hasOwn(sealed, 'x');

let traps = 0;
Object.setPrototypeOf(proto, new Proxy({}, {
  set(target, key, value, receiver) {
    traps++;
    return Reflect.set(target, key, value, receiver);
  }
}));
const proxied = Object.create(proto);
put(proxied, 17);
JSON.stringify([checksum, readonly, setters, last, observed.side,
                own.x, nonextensible, traps, proxied.x]);
