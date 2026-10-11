// Method lookup precedes mutable reads, coercion and derived-this checks.
let checks = 0;
function check(value, label) {
  if (!value) throw new Error(label);
  checks++;
}
function identity(value) { return value; }

function capturedRead(computed) {
  let value = 1;
  const events = [];
  const receiver = {
    get m() {
      events.push("get");
      value = 9;
      return function (argument) { events.push(argument); return argument; };
    }
  };
  const result = computed ? receiver["m"](value) : receiver.m(value);
  return [events, result];
}
function numericCoercion(kind) {
  const events = [];
  const receiver = {
    get m() {
      events.push("get");
      return function (argument) { events.push(argument); return argument; };
    }
  };
  const candidate = { valueOf() { events.push("coerce"); return 4; } };
  if (kind === 0) receiver.m(+candidate);
  else if (kind === 1) receiver.m(-candidate);
  else receiver.m(~candidate);
  return events;
}
function bigintPlus(computed) {
  const events = [];
  const receiver = { get m() { events.push("get"); return identity; } };
  try {
    if (computed) receiver["m"](+1n);
    else receiver.m(+1n);
  } catch (error) { events.push(error.name); }
  return events;
}

let savedArguments;
const mappedReceiver = {
  get m() { savedArguments[0] = 9; return identity; }
};
function mappedRead(value) {
  savedArguments = arguments;
  return mappedReceiver.m(value);
}
const legacyReceiver = {
  get m() { legacyRead.arguments[0] = 9; return identity; }
};
function legacyRead(value) { return legacyReceiver.m(value); }

function withRead() {
  let value = 1;
  const environment = {};
  const receiver = { get m() { environment.value = 9; return identity; } };
  with (environment) { return receiver.m(value); }
}
function evalRead() {
  let value = 1;
  const change = () => eval("value = 9");
  const receiver = { get m() { change(); return identity; } };
  return receiver.m(value);
}
function immutableRead() {
  const value = 7;
  const receiver = { get m() { return argument => value + argument; } };
  return receiver.m(value);
}
const stableReceiver = { get m() { return identity; } };
function privateRead(value) {
  let local = value;
  return stableReceiver.m(local);
}
function ordinaryThis() { return stableReceiver.m(this); }
function safeUnary(value) {
  return [stableReceiver.m(-1), stableReceiver.m(+"3"), stableReceiver.m(~1n),
    stableReceiver.m(-1n), stableReceiver.m(+(!value)), stableReceiver.m(void value)];
}

let derivedEvents = [];
class Base {}
class Derived extends Base {
  constructor() {
    const initialize = () => super();
    const receiver = {
      get m() {
        derivedEvents.push("get");
        initialize();
        return function (argument) { derivedEvents.push(argument instanceof Derived); };
      }
    };
    receiver.m(this);
  }
}
class ArrowDerived extends Base {
  constructor() {
    const initialize = () => super();
    const receiver = {
      get m() {
        derivedEvents.push("arrow-get");
        initialize();
        return function (argument) { derivedEvents.push(argument instanceof ArrowDerived); };
      }
    };
    const invoke = () => receiver.m(this);
    invoke();
  }
}
for (let i = 0; i < 10000; i++) {
  capturedRead((i & 1) === 0);
  numericCoercion(i % 3);
  bigintPlus((i & 1) === 0);
  mappedRead(1);
  legacyRead(1);
  withRead();
  immutableRead();
  privateRead(i);
}
for (let i = 0; i < 2000; i++) {
  derivedEvents = [];
  new Derived();
  new ArrowDerived();
}

check(JSON.stringify(capturedRead(false)) === '[["get",9],9]', "captured static read");
check(JSON.stringify(capturedRead(true)) === '[["get",9],9]', "captured computed read");
check(JSON.stringify(numericCoercion(0)) === '["get","coerce",4]', "unary plus order");
check(JSON.stringify(numericCoercion(1)) === '["get","coerce",-4]', "unary minus order");
check(JSON.stringify(numericCoercion(2)) === '["get","coerce",-5]', "bitwise not order");
check(JSON.stringify(bigintPlus(false)) === '["get","TypeError"]', "bigint static throw");
check(JSON.stringify(bigintPlus(true)) === '["get","TypeError"]', "bigint computed throw");
check(mappedRead(1) === 9, "escaped mapped arguments read");
check(legacyRead(1) === 1, "legacy argument snapshot");
check(withRead() === 9, "getter installs with binding");
check(evalRead() === 9, "nested direct eval changes captured binding");
check(immutableRead() === 14 && privateRead(23) === 23, "stable binding admission");
const originalThis = {};
check(ordinaryThis.call(originalThis) === originalThis, "ordinary this remains stable");
const safe = safeUnary({ valueOf() { throw new Error("unobserved coercion"); } });
check(safe[0] === -1 && safe[1] === 3 && safe[2] === -2n && safe[3] === -1n
  && safe[4] === 0 && safe[5] === undefined, "primitive unary admission");
derivedEvents = [];
new Derived();
new ArrowDerived();
check(JSON.stringify(derivedEvents) === '["get",true,"arrow-get",true]', "derived this after getter");
console.log("method argument order checks", checks);
