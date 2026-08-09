function ownMethod(value) {
  if (value < 0) throw "method-boom";
  return this.base + value;
}

function prototypeMethod(value) {
  return this.base + value;
}

const ownReceiver = { base: 40, method: ownMethod };
const methodPrototype = { method: prototypeMethod };
const prototypeReceiver = Object.create(methodPrototype);
prototypeReceiver.base = 20;

function invokeOwn(receiver, value) {
  return receiver.method(value);
}

function invokePrototype(receiver, value) {
  return receiver.method(value);
}

for (let i = 0; i < 5000; i++) {
  invokeOwn(ownReceiver, i);
  invokePrototype(prototypeReceiver, i);
}

let caught = "missing";
try {
  invokeOwn(ownReceiver, -1);
} catch (error) {
  caught = error;
}

let accessorEffects = 0;
const accessorReceiver = {
  base: 10,
  get method() {
    accessorEffects++;
    return ownMethod;
  }
};
const missed = invokeOwn(accessorReceiver, 5);

function recursive(self, value) {
  if (value <= 0) return value;
  return self(self, value - 1);
}
for (let i = 0; i < 5000; i++) recursive(recursive, 4);

globalThis.__methodStressSink = [];
function allocatingMethod(count) {
  let checksum = 0;
  for (let i = 0; i < count; i++) {
    const item = { value: i, padding: "method-stress-" + i };
    globalThis.__methodStressSink.push(item);
    checksum += item.value & 1;
  }
  return this.marker + checksum;
}
const allocatingReceiver = { marker: "root:", method: allocatingMethod };
function allocateThroughMethod(receiver, count) {
  return receiver.method(count);
}
for (let i = 0; i < 5000; i++) allocateThroughMethod(allocatingReceiver, 0);

JSON.stringify({
  own: invokeOwn(ownReceiver, 2),
  prototype: invokePrototype(prototypeReceiver, 2),
  caught,
  missed,
  accessorEffects,
  recursive: recursive(recursive, 64),
  allocation: allocateThroughMethod(allocatingReceiver, 200)
});
