// Step 9 widening: virtual object identity and materialization must remain
// exact across escape, reentry, exceptions, weak observation, OSR and GC.

function localArray(left, right) {
  const values = [left, right, left + right];
  return values[0] + values[1] + values[2];
}

function scalarOnce(value) {
  const local = [value];
  return local[0] + 1;
}

function localObjectIdentity() {
  const local = {};
  return local === local;
}

function identity(flag) {
  const first = [1, 2, 3];
  const alias = first;
  const second = [1, 2, 3];
  return [alias === first, first === second, flag ? first : second];
}

function external(value) {
  return [value[0], value[1], value.length];
}

function escapeToCall(left, right) {
  const values = [left, right];
  return external(values);
}

function capture(value) {
  const object = { value };
  return function readCaptured() {
    return object.value;
  };
}

let getterReads = 0;
let setterWrites = 0;
const accessor = {
  get value() {
    getterReads++;
    return 17;
  },
  set value(next) {
    setterWrites += next;
  },
};
const proxy = new Proxy(accessor, {
  get(target, key, receiver) {
    return Reflect.get(target, key, receiver);
  },
  set(target, key, value, receiver) {
    return Reflect.set(target, key, value, receiver);
  },
});

function accessorAndProxy(value) {
  const boxed = [value];
  proxy.value = boxed[0];
  return proxy.value + boxed[0];
}

class ThrowsAfterObservation {
  constructor(value) {
    this.value = value[0];
    throw new RangeError("pea-" + value[1]);
  }
}

function constructorThrow(value) {
  const arguments = [value, value + 1];
  try {
    new ThrowsAfterObservation(arguments);
  } catch (error) {
    return error.message + ":" + arguments[0];
  }
  return "unreachable";
}

function deoptLeaf(value) {
  if (typeof value !== "number") return value[0];
  return value + 1;
}

function deoptMiddle(value, flip) {
  const state = [value, value + 1];
  return deoptLeaf(flip ? state : state[0]) + state[1];
}

function allocationPressure(seed) {
  const kept = [seed, seed + 1, seed + 2];
  for (let index = 0; index < 4; index++) {
    const garbage = [index, { index }, "pea-" + index];
    if (garbage[0] === -1) return garbage;
  }
  return kept[0] + kept[1] + kept[2];
}

function osrVirtual(rounds, escapeAt) {
  let checksum = 0;
  for (let index = 0; index < rounds; index++) {
    const pair = [index, index + 1];
    checksum += pair[0] + pair[1];
    if (index === escapeAt) checksum += external(pair)[2];
  }
  return checksum;
}

function conditionalAlias(flag, value) {
  const first = [value, value + 1];
  const second = first;
  const chosen = flag ? second : [value + 2, value + 3];
  return [chosen[0], chosen === first, first[1]];
}

for (let warm = 0; warm < 256; warm++) {
  localArray(warm, 2);
  scalarOnce(warm);
  localObjectIdentity();
  identity((warm & 1) === 0);
  escapeToCall(warm, warm + 1);
  accessorAndProxy(3);
  constructorThrow(4);
  deoptMiddle(warm, false);
  allocationPressure(warm);
  conditionalAlias((warm & 1) === 0, warm);
}

const captured = capture(23);
const identityResult = identity(true);
const weakTarget = identityResult[2];
const weak = new WeakRef(weakTarget);
const finalizationRegistry = new FinalizationRegistry(() => {});
finalizationRegistry.register(weakTarget, "pea-held-value");
const step9Result = JSON.stringify({
  local: localArray(20, 22),
  localDeopt: localArray("20", 2),
  scalarDeopt: scalarOnce("40"),
  localObjectIdentity: localObjectIdentity(),
  identity: [identityResult[0], identityResult[1], identityResult[2][2]],
  escaped: escapeToCall(7, 8),
  captured: captured(),
  accessor: accessorAndProxy(5),
  accessorCounts: [getterReads, setterWrites],
  thrown: constructorThrow(9),
  nestedDeopt: deoptMiddle(10, true),
  pressure: allocationPressure(30),
  osr: osrVirtual(4200, 4097),
  aliases: [conditionalAlias(true, 11), conditionalAlias(false, 11)],
  weak: weak.deref() === weakTarget,
  finalization: typeof finalizationRegistry.unregister === "function",
});
console.log(step9Result);
step9Result;
