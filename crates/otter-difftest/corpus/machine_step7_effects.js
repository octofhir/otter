// Step 7 widening: value numbering must not reuse heap proofs or payloads
// across stores, calls, accessor/Proxy reentry, throws, or moving allocation.

function readPair(receiver) {
  return [receiver.value, receiver.value];
}

function readAcrossCall(receiver, callback) {
  const before = receiver.value;
  callback(receiver);
  const after = receiver.value;
  return [before, after];
}

function readAcrossStore(receiver, value) {
  const before = receiver.value;
  receiver.value = value;
  const after = receiver.value;
  return [before, after];
}

function elementAcrossStore(receiver, index, value) {
  const before = receiver[index];
  receiver[index] = value;
  const after = receiver[index];
  return [before, after];
}

function commonArithmetic(left, right) {
  const first = left * right + 3;
  const second = left * right + 3;
  return first - second;
}

function duplicateDecode(receiver) {
  const value = receiver.value;
  return value * 2 + value * 2;
}

const warmRecord = { value: 1 };
const warmRecordB = { padding: 0, value: 2 };
const warmArray = [1, 2, 3, 4];
function noMutation() {}
for (let warm = 0; warm < 5000; warm++) {
  readPair((warm & 1) === 0 ? warmRecord : warmRecordB);
  readAcrossCall(warmRecord, noMutation);
  readAcrossStore(warmRecord, warm & 255);
  elementAcrossStore(warmArray, warm & 3, warm & 255);
  commonArithmetic(warm & 255, 7);
  duplicateDecode(warmRecord);
}

let getterCalls = 0;
const accessor = {};
Object.defineProperty(accessor, "value", {
  configurable: true,
  get() {
    getterCalls++;
    return getterCalls * 10;
  },
});

let proxyGets = 0;
const proxy = new Proxy({ value: 7 }, {
  get(target, key) {
    if (key === "value") proxyGets++;
    return target[key] + proxyGets;
  },
});

const calledRecord = { value: 20 };
let callbackCalls = 0;
function mutate(receiver) {
  callbackCalls++;
  receiver.value = 21;
  const churn = [];
  for (let index = 0; index < 64; index++) {
    churn.push({ index, payload: "gvn-" + index });
  }
}

const storedRecord = { value: 30 };
const storedArray = [40, 41, 42];

let throwGets = 0;
const throwing = {};
Object.defineProperty(throwing, "value", {
  get() {
    throwGets++;
    if (throwGets === 2) throw new RangeError("second read");
    return 50;
  },
});
let thrown = "missing";
try {
  readPair(throwing);
} catch (error) {
  thrown = error.name;
}

const step7Result = JSON.stringify({
  accessor: readPair(accessor),
  getterCalls,
  proxy: readPair(proxy),
  proxyGets,
  call: readAcrossCall(calledRecord, mutate),
  callbackCalls,
  store: readAcrossStore(storedRecord, 31),
  element: elementAcrossStore(storedArray, 1, 99),
  commonArithmetic: commonArithmetic(6, 7),
  duplicateDecode: duplicateDecode({ value: 9 }),
  thrown,
  throwGets,
});
console.log(step7Result);
step7Result;
