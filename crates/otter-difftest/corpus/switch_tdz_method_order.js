// A selector may enter a clause without initializing an earlier lexical name.
let checks = 0;
function check(actual, expected, label) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) throw new Error(label);
  checks++;
}
function methodReceiver(events) {
  return { get m() { events.push("get"); return value => events.push(value); } };
}
function uncaptured(entry) {
  const events = [];
  const receiver = methodReceiver(events);
  try {
    switch (entry) {
      case 0: const value = 1;
      case 1: receiver.m(value);
    }
  } catch (error) { events.push(error.name); }
  return events;
}
function captured(entry) {
  const events = [];
  const receiver = methodReceiver(events);
  try {
    switch (entry) {
      case 0: const value = 1; const keep = () => value; keep();
      case 1: receiver.m(value);
    }
  } catch (error) { events.push(error.name); }
  return events;
}
function outerClosure(entry) {
  const events = [];
  const receiver = methodReceiver(events);
  try {
    switch (entry) {
      case 0: const value = 1;
      case 1: const invoke = function inner() { receiver.m(value); }; invoke();
    }
  } catch (error) { events.push(error.name); }
  return events;
}
function callerChain() {
  const events = [];
  const receiver = methodReceiver(events);
  try { const value = eval("receiver.m(value)"); }
  catch (error) { events.push(error.name); }
  return events;
}
function mutable(entry, operation) {
  const events = [];
  try {
    switch (entry) {
      case 0: let value = 1;
      case 1:
        if (operation === "read") events.push(value);
        else if (operation === "update") { value++; events.push(value); }
        else if (operation === "compound") {
          value += (events.push("rhs"), 2); events.push(value);
        } else if (operation === "assign") {
          value = (events.push("rhs"), 9); events.push(value);
        } else if (operation === "array") {
          [value] = (events.push("rhs"), [9]); events.push(value);
        } else {
          ({[(events.push("key"), "x")]: value} = (events.push("rhs"), {
            get x() { events.push("get"); return 9; }
          }));
          events.push(value);
        }
    }
  } catch (error) { events.push(error.name); }
  return events;
}
function immutable(entry, operation) {
  const events = [];
  try {
    switch (entry) {
      case 0: const value = operation === "update" ? {
        valueOf() { events.push("coerce"); return 1; }
      } : 1;
      case 1:
        if (operation === "assign") value = (events.push("rhs"), 9);
        else if (operation === "update") value++;
        else if (operation === "array") [value] = (events.push("rhs"), [9]);
        else ({[(events.push("key"), "x")]: value} = (events.push("rhs"), {
          get x() { events.push("get"); return 9; }
        }));
    }
  } catch (error) { events.push(error.name); }
  return events;
}
function reentry(entries) {
  const events = [];
  for (let i = 0; i < entries.length; i++) {
    try {
      switch (entries[i]) {
        case 0: let value = i;
        case 1: events.push(value); break;
      }
    } catch (error) { events.push(error.name); }
  }
  return events;
}
function privateClause(entry) {
  switch (entry) {
    case 0: const value = 7; return value;
    case 1: let other = 8; other++; return other;
  }
}
function withImmutableShadow() {
  const value = 1;
  const target = { value: 7 };
  with (target) { value++; }
  return [value, target.value];
}
function ownDestructureTdz() {
  try { let value = [value] = [3]; }
  catch (error) { return error.name; }
}
const mutableOps = ["read", "update", "compound", "assign", "array", "computed"];
const immutableOps = ["assign", "update", "array", "computed"];
for (let i = 0; i < 10000; i++) {
  uncaptured(i & 1);
  captured(i & 1);
  outerClosure(i & 1);
  mutable(i & 1, mutableOps[i % mutableOps.length]);
  immutable(i & 1, immutableOps[i % immutableOps.length]);
  privateClause(i & 1);
}
for (const target of [uncaptured, captured, outerClosure]) {
  check(target(0), ["get", 1], "initialized fallthrough method");
  check(target(1), ["get", "ReferenceError"], "method before cross-clause TDZ");
}
check(callerChain(), ["get", "ReferenceError"], "eval chain const has no live initialization proof");
const successful = [[1], [2], ["rhs", 3], ["rhs", 9], ["rhs", 9], ["rhs", "key", "get", 9]];
const uninitialized = [["ReferenceError"], ["ReferenceError"], ["ReferenceError"],
  ["rhs", "ReferenceError"], ["rhs", "ReferenceError"], ["rhs", "key", "get", "ReferenceError"]];
for (let i = 0; i < mutableOps.length; i++) {
  check(mutable(0, mutableOps[i]), successful[i], "initialized mutable " + mutableOps[i]);
  check(mutable(1, mutableOps[i]), uninitialized[i], "uninitialized mutable " + mutableOps[i]);
}
const immutableInitialized = [["rhs", "TypeError"], ["coerce", "TypeError"],
  ["rhs", "TypeError"], ["rhs", "key", "get", "TypeError"]];
const immutableUninitialized = [["rhs", "ReferenceError"], ["ReferenceError"],
  ["rhs", "ReferenceError"], ["rhs", "key", "get", "ReferenceError"]];
for (let i = 0; i < immutableOps.length; i++) {
  check(immutable(0, immutableOps[i]), immutableInitialized[i], "initialized immutable " + immutableOps[i]);
  check(immutable(1, immutableOps[i]), immutableUninitialized[i], "uninitialized immutable " + immutableOps[i]);
}
check(reentry([0, 1, 0, 1]), [0, "ReferenceError", 2, "ReferenceError"], "fresh switch context on each entry");
check([privateClause(0), privateClause(1)], [7, 9], "clause-private register values");
check(withImmutableShadow(), [1, 8], "with shadow excludes immutable update proof");
check(ownDestructureTdz(), "ReferenceError", "assignment does not initialize declaration TDZ");

function withDestructure(kind) {
  const events = [];
  const value = 1;
  const target = {value: 7};
  let caught = "none";
  try {
    with (target) {
      if (kind === 0) [value] = [9];
      else if (kind === 1) ({value} = {value: 9});
      else if (kind === 2) ({x: value} = {x: 9});
      else if (kind === 3) [value = (events.push("default"), 9)] = [undefined];
      else if (kind === 4) ({value = (events.push("default"), 9)} = {});
      else if (kind === 5) [...value] = [9, 10];
      else if (kind === 6) ({...value} = {x: 9});
      else ({x: value} = {get x() { events.push("get"); return 9; }});
    }
  } catch (error) { caught = error.name; }
  return [value, target.value, events, caught];
}
function withLogical(operation, writes) {
  const events = [];
  const value = 1;
  const initial = operation === 0 ? (writes ? 0 : 7)
    : operation === 1 ? (writes ? 7 : 0) : (writes ? null : 7);
  const target = {value: initial};
  let caught = "none";
  try {
    with (target) {
      if (operation === 0) value ||= (events.push("rhs"), 9);
      else if (operation === 1) value &&= (events.push("rhs"), 9);
      else value ??= (events.push("rhs"), 9);
    }
  } catch (error) { caught = error.name; }
  return [value, target.value, events, caught];
}
function unresolvedLogical(operation) {
  const events = [];
  let caught = "none";
  try {
    if (operation === 0) __missingLogicalProof__ ||= (events.push("rhs"), 9);
    else if (operation === 1) __missingLogicalProof__ &&= (events.push("rhs"), 9);
    else __missingLogicalProof__ ??= (events.push("rhs"), 9);
  } catch (error) { caught = error.name; }
  return [caught, Object.prototype.hasOwnProperty.call(globalThis, "__missingLogicalProof__"), events];
}
function blockedWith(operation, blocker) {
  const value = 1;
  const target = {value: 7, [Symbol.unscopables]: blocker};
  let caught = "none";
  try {
    with (target) {
      if (operation === 0) value = 9;
      else if (operation === 1) [value] = [9];
      else value &&= 9;
    }
  } catch (error) { caught = error.name; }
  return [value, target.value, caught];
}
function selfNumeric(big) {
  const events = [];
  const callable = function self() { return [++self, --self, self++, self--]; };
  callable[Symbol.toPrimitive] = function (hint) {
    events.push(hint); return big ? 3n : 3;
  };
  return [callable().map(value => String(value)), events];
}
function selfDiscarded() {
  let effects = 0;
  const callable = function self() { ++self; self--; return self; };
  callable[Symbol.toPrimitive] = function () { effects++; return 3; };
  return [callable() === callable, effects];
}
function strictSelf() {
  let effects = 0;
  const callable = function self() { "use strict"; return ++self; };
  callable[Symbol.toPrimitive] = function () { effects++; return 3; };
  try { callable(); } catch (error) { return [error.name, effects]; }
}
function memberReferenceOrder(kind) {
  const events = [];
  const target = {set x(value) { events.push("set:" + value); }};
  function base() { events.push("base"); return target; }
  function fallback() { events.push("default"); return 9; }
  const source = {get a() { events.push("get"); return undefined; }};
  if (kind === 0) ({a: base().x = fallback()} = source);
  else if (kind === 1) {
    const iterable = {
      [Symbol.iterator]() { return this; },
      next() { events.push("get"); return {value: undefined, done: false}; },
      return() { events.push("close"); return {done: true}; }
    };
    [base().x = fallback()] = iterable;
  } else {
    const restTarget = {set x(value) { events.push("set:" + value.a); }};
    function restBase() { events.push("base"); return restTarget; }
    ({...restBase().x} = {get a() { events.push("get"); return 9; }});
  }
  return events;
}
for (let i = 0; i < 10000; i++) {
  withDestructure(i % 8);
  withLogical(i % 3, (i & 1) === 0);
  unresolvedLogical(i % 3);
  selfNumeric((i & 1) === 0);
  selfDiscarded();
}
const destructured = [9, 9, 9, 9, 9, [9, 10], {x: 9}, 9];
for (let i = 0; i < destructured.length; i++) {
  const effects = i === 3 || i === 4 ? ["default"] : i === 7 ? ["get"] : [];
  check(withDestructure(i), [1, destructured[i], effects, "none"], "selected with destructuring " + i);
}
for (let i = 0; i < 3; i++) {
  check(withLogical(i, true), [1, 9, ["rhs"], "none"], "selected logical with write " + i);
  check(withLogical(i, false), [1, i === 1 ? 0 : 7, [], "none"], "selected logical with short circuit " + i);
  check(unresolvedLogical(i), ["ReferenceError", false, []], "unresolved logical GetValue " + i);
}
const blocker = function () {};
blocker.value = true;
for (let i = 0; i < 3; i++) {
  check(blockedWith(i, blocker), [1, 7, "TypeError"], "callable unscopables " + i);
}
check(blockedWith(0, new Proxy(blocker, {})), [1, 7, "TypeError"], "callable proxy unscopables");
const revoked = Proxy.revocable(blocker, {});
revoked.revoke();
check(blockedWith(0, revoked.proxy), [1, 7, "TypeError"], "revoked unscopables lookup");
for (const primitive of [null, 7, "blocked"]) {
  check(blockedWith(0, primitive), [1, 9, "none"], "nonobject unscopables");
}
const expectedNumeric = [["4", "2", "3", "3"], ["number", "number", "number", "number"]];
check(selfNumeric(false), expectedNumeric, "self numeric prefix and postfix");
check(selfNumeric(true), expectedNumeric, "self bigint prefix and postfix");
check(selfDiscarded(), [true, 2], "ignored self stores keep coercion once");
check(strictSelf(), ["TypeError", 1], "strict self update still rejects store");
check(memberReferenceOrder(0), ["base", "get", "default", "set:9"], "object member reference before default");
check(memberReferenceOrder(1), ["base", "get", "default", "set:9", "close"], "array member reference before next");
check(memberReferenceOrder(2), ["base", "get", "set:9"], "rest member reference before getters");
console.log("switch TDZ order checks", checks);
