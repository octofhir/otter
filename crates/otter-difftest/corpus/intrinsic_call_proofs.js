// Ordinary resolution, evaluated-callable proofs and exact pre-effect misses.
const SavedArray = Array, SavedObject = Object, SavedNumber = Number;
const SavedBoolean = Boolean, SavedFunction = Function, SavedError = Error;
const SavedTypeError = TypeError, SavedMath = Math, SavedUint8Array = Uint8Array;
let checks = 0;
function check(value, label) {
  if (!value) throw new SavedError(label);
  checks++;
}
const coldArrayPrototype = SavedArray.prototype;
coldArrayPrototype.length = "3";
check(typeof coldArrayPrototype.length === "number" && coldArrayPrototype.length === 3,
  "cold array prototype length coercion");
coldArrayPrototype.length = 0;
function arrayCall() { return Array(); }
function arrayNew() { return new Array(); }
function arrayLength(value) { return new Array(value); }
function objectCall() { return Object(); }
function objectNew() { return new Object(); }
function booleanCall(value, ignored) { return Boolean(value, ignored); }
function numberCall() { return Number(); }
function aliasArray(fn) { return new fn(); }
for (let i = 0; i < 6000; i++) {
  arrayCall(); arrayNew(); arrayLength(3); objectCall(); objectNew();
  booleanCall(i, i); numberCall(); aliasArray(SavedArray);
}
const a = arrayCall(), b = arrayNew(), o = objectCall(), p = objectNew();
check(a !== b && a.length === 0 && b.length === 0, "fresh arrays");
check(SavedObject.getPrototypeOf(a) === SavedArray.prototype, "array prototype");
check(o !== p && SavedObject.getPrototypeOf(o) === SavedObject.prototype, "fresh objects");
check(numberCall() === 0 && booleanCall(0, 9) === false, "scalar calls");
check(Array(3).length === 3 && !(0 in arrayLength(3)), "length holes");
check(arrayLength("3")[0] === "3", "string array item");
check(arrayLength(3n)[0] === 3n, "bigint array item");
const item = {valueOf() { throw "unexpected coercion"; }};
check(arrayLength(item)[0] === item, "object array item");
check(Array(2, 4).join() === "2,4", "multiple arguments");
let argumentEffects = 0;
function effect(value) { argumentEffects++; return value; }
for (let i = 0; i < 6000; i++) booleanCall(true, false);
check(booleanCall(effect(0), effect(1)) === false && argumentEffects === 2, "ignored argument effects");
for (const invalid of [-1, 0.5, NaN, Infinity, 4294967296]) {
  let name = "";
  try { arrayLength(effect(invalid)); } catch (error) { name = error.name; }
  check(name === "RangeError", "invalid length");
}
check(argumentEffects === 7 && arrayLength(4).length === 4, "length miss once");
const wideArray = arrayLength(effect(2147483648));
check(wideArray.length === 2147483648 && !(0 in wideArray) && argumentEffects === 8,
  "valid length outside Int32 domain");

function Replacement(...args) { this.tag = 7; return {tag: 7, count: args.length}; }
function replacementScalar() { return 7; }
globalThis.Array = Replacement;
check(arrayCall().tag === 7 && arrayNew().tag === 7, "array late replacement");
check(aliasArray(SavedArray).length === 0, "retained array alias");
globalThis.Array = SavedArray;
globalThis.Object = Replacement;
check(objectCall().tag === 7 && objectNew().tag === 7, "object late replacement");
globalThis.Object = SavedObject;
globalThis.Boolean = function (...args) {
  return args.length === 0 ? 42 : args.length === 2 ? args[0] + args[1] : -1;
};
check(booleanCall(effect(3), effect(4)) === 7 && Boolean() === 42 && argumentEffects === 10,
  "boolean replacement retains ignored operands");
globalThis.Boolean = SavedBoolean;
globalThis.Number = replacementScalar;
check(numberCall() === 7 && Number() === 7, "number replacement");
globalThis.Number = SavedNumber;

let getters = 0;
const arrayDescriptor = SavedObject.getOwnPropertyDescriptor(globalThis, "Array");
SavedObject.defineProperty(globalThis, "Array", {
  configurable: true,
  get() { getters++; return getters % 2 ? SavedArray : Replacement; }
});
argumentEffects = 0;
const first = arrayLength(effect(2)), second = arrayLength(effect(2));
check(first.length === 2 && second.tag === 7, "getter callable changes");
check(getters === 2 && argumentEffects === 2, "getter and args exactly once");
SavedObject.defineProperty(globalThis, "Array", {
  configurable: true, get() { getters++; throw "callee"; }
});
let thrown = "";
try { new Array(effect(2)); } catch (error) { thrown = error; }
check(thrown === "callee" && getters === 3 && argumentEffects === 2, "callee before args");
SavedObject.defineProperty(globalThis, "Array", arrayDescriptor);

function dynamicNew(scope) { with (scope) { return [new Array(), new Object()]; } }
const dynamic = dynamicNew({Array: Replacement, Object: Replacement});
check(dynamic[0].tag === 7 && dynamic[1].tag === 7, "with construct resolution");
function evalArray() { eval("var Array = Replacement;"); return new Array(); }
function evalNumber() { eval("var Number = replacementScalar;"); return Number(); }
check(evalArray().tag === 7 && evalNumber() === 7, "eval binding resolution");
let withCalls = 0;
const scope = {Array() { withCalls++; return this === scope; }};
function withCall(scope) { with (scope) { return Array(); } }
check(withCall(scope) && withCalls === 1, "with receiver");

globalThis.Function = Replacement;
check(Function("x").tag === 7 && new Function("x").tag === 7, "function replacement");
globalThis.Function = SavedFunction;
check(Function("return 11")() === 11, "function canonical fallback");
globalThis.Error = Replacement;
check(Error === Replacement && Error("x").tag === 7 && new Error("x").tag === 7, "error replacement");
check(typeof Error === "function", "error typeof resolution");
delete globalThis.Error;
check(typeof Error === "undefined", "deleted error typeof");
globalThis.Error = SavedError;
globalThis.TypeError = Replacement;
let internalError;
try { const {x} = null; } catch (error) { internalError = error; }
check(internalError instanceof SavedTypeError, "internal error ownership");
check(new TypeError("x").tag === 7, "typeerror replacement");
globalThis.TypeError = SavedTypeError;

const arrayIsArray = SavedArray.isArray, arrayFrom = SavedArray.from, arrayOf = SavedArray.of;
let staticGets = 0, staticCalls = 0;
function customStatic(...args) { staticCalls++; return [this === SavedArray, args.length]; }
SavedObject.defineProperty(SavedArray, "isArray", {configurable: true, get() { staticGets++; return customStatic; }});
check(Array.isArray([]).join() === "true,1" && staticGets === 1 && staticCalls === 1, "array static getter");
SavedObject.defineProperty(SavedArray, "isArray", {configurable: true, writable: true, value: arrayIsArray});
SavedArray.from = customStatic; SavedArray.of = customStatic;
check(Array.from([1]).join() === "true,1" && Array.of(1, 2).join() === "true,2", "array static replacement");
SavedArray.from = arrayFrom; SavedArray.of = arrayOf;
check(arrayIsArray(a) && arrayIsArray(SavedArray.prototype), "canonical array identity");
check(arrayIsArray(new Proxy(new Proxy(SavedArray.prototype, {}), {})), "proxy array prototype identity");
const arrayPrototype = SavedArray.prototype;
arrayPrototype.length = "3";
check(typeof arrayPrototype.length === "number" && arrayPrototype.length === 3,
  "array prototype length coercion");
arrayPrototype[5] = 7;
check(arrayPrototype.length === 6 && arrayPrototype[5] === 7, "array prototype index growth");
arrayPrototype.length = 1;
check(arrayPrototype.length === 1 && !SavedObject.hasOwn(arrayPrototype, "5"), "array prototype truncation");
let prototypeLengthError = "";
try { SavedObject.defineProperty(arrayPrototype, "length", {value: -1}); }
catch (error) { prototypeLengthError = error.name; }
check(prototypeLengthError === "RangeError" && arrayPrototype.length === 1, "array prototype invalid length");
let lengthCoercions = 0;
arrayPrototype.length = {valueOf() { lengthCoercions++; return 2; }};
check(arrayPrototype.length === 2 && lengthCoercions === 2, "array prototype length double coercion");
arrayPrototype.length = 0;
const prototypeClone = structuredClone(SavedArray.prototype);
check(arrayIsArray(prototypeClone) && prototypeClone !== SavedArray.prototype &&
  SavedObject.getPrototypeOf(prototypeClone) === SavedArray.prototype, "array prototype clone identity");
check(!arrayIsArray(structuredClone(SavedObject.create(SavedArray.prototype))), "clone brand is not inherited");
check(JSON.stringify(SavedArray.prototype) === "[]" && JSON.stringify(new Proxy(SavedArray.prototype, {})) === "[]", "array prototype json semantics");
check([1].concat(new Proxy(SavedArray.prototype, {})).join() === "1", "array prototype spread semantics");
const revoked = Proxy.revocable([], {});
revoked.revoke();
let revokedName = "";
try { arrayIsArray(revoked.proxy); } catch (error) { revokedName = error.name; }
check(revokedName === "TypeError", "revoked array proxy throw");
function shadowArray(Array) { return [Array.isArray(1), Array.from(1), Array.of(1)]; }
const shadow = {isArray() { return 7; }, from() { return 8; }, of() { return 9; }};
check(shadowArray(shadow).join() === "7,8,9", "array static lexical owner");
const prototype = SavedArray.prototype;
const prototypeKeys = SavedObject.getOwnPropertyNames(prototype);
const followingKey = prototypeKeys[prototypeKeys.indexOf("push") + 1];
const pushDescriptor = SavedObject.getOwnPropertyDescriptor(prototype, "push");
const followingDescriptor = SavedObject.getOwnPropertyDescriptor(prototype, followingKey);
prototype[followingKey] = prototype.push;
let shapeEffects = 0;
function arrayShapeCall(array, value) { shapeEffects++; return array.push(value); }
for (let i = 0; i < 6000; i++) arrayShapeCall([], 1);
shapeEffects = 0;
delete prototype.push;
let absentMethodName = "";
try { arrayShapeCall([], 7); } catch (error) { absentMethodName = error.name; }
SavedObject.defineProperty(prototype, "push", pushDescriptor);
SavedObject.defineProperty(prototype, followingKey, followingDescriptor);
check(absentMethodName === "TypeError" && shapeEffects === 1, "stale holder slot never changes method name");
check(arrayShapeCall([], 7) === 1 && shapeEffects === 2, "restored method once");
function CustomArray(length) { this.initialLength = length; }
const fromCustom = arrayFrom.call(CustomArray, {0: 5, length: 1});
check(fromCustom instanceof CustomArray && fromCustom[0] === 5 && fromCustom.length === 1, "array from receiver");

const queueDescriptor = SavedObject.getOwnPropertyDescriptor(globalThis, "queueMicrotask");
let queueArgumentCount = 0;
globalThis.queueMicrotask = function (callback) { queueArgumentCount += arguments.length; return callback(); };
check(queueMicrotask(function () { return 19; }) === 19, "queue global callable resolution");
check(queueArgumentCount === 1, "queue evaluated arguments delivered");
function shadowQueue(queueMicrotask) { return queueMicrotask(function () {}); }
check(shadowQueue(function () { return 23; }) === 23, "queue lexical callable resolution");
let queueEffects = 0, queueThrown = 0;
SavedObject.defineProperty(globalThis, "queueMicrotask", {configurable: true, get() { throw 37; }});
try { queueMicrotask((queueEffects++, function () {})); } catch (error) { queueThrown = error; }
SavedObject.defineProperty(globalThis, "queueMicrotask", queueDescriptor);
check(queueThrown === 37, "queue callable getter throw");
check(queueEffects === 0, "queue resolution before argument effects");

const callableReceiver = function () {};
let callableGetterEffects = 0;
SavedObject.defineProperty(callableReceiver, "observed", {get() {
  callableGetterEffects++;
  for (let i = 0; i < 2400; i++) callableReceiver.retained = {index: i};
  return function (value) { return this === callableReceiver ? value * 2 : -1; };
}});
check(callableReceiver.observed(7) === 14, "callable getter preserves moving receiver");
check(callableReceiver.observed(...[7]) === 14, "spread callable getter preserves moving receiver");
check(callableGetterEffects === 2, "collecting callable getters execute once");

function constants(Math, Number, Uint8Array) { return [Math.PI, Number.MAX_VALUE, Uint8Array.BYTES_PER_ELEMENT]; }
check(constants({PI: 7}, {MAX_VALUE: 8}, {BYTES_PER_ELEMENT: 9}).join() === "7,8,9", "constant lexical resolution");
globalThis.Math = {PI: 7}; globalThis.Number = {MAX_VALUE: 8}; globalThis.Uint8Array = {BYTES_PER_ELEMENT: 9};
check([Math.PI, Number.MAX_VALUE, Uint8Array.BYTES_PER_ELEMENT].join() === "7,8,9", "constant global resolution");
globalThis.Math = SavedMath; globalThis.Number = SavedNumber; globalThis.Uint8Array = SavedUint8Array;
function localConstants(NaN, Infinity) { return [NaN, Infinity]; }
check(localConstants(7, 8).join() === "7,8", "immutable global lexical shadows");
function evalConstants() { eval("var NaN = 7; var Infinity = 8;"); return [NaN, Infinity]; }
check(evalConstants().join() === "7,8", "immutable global eval shadows");

check(new Boolean(false).valueOf() === false && new Number(3).valueOf() === 3, "wrapper construct fallback");
class DerivedArray extends SavedArray {}
const derived = new DerivedArray(2);
check(derived.length === 2 && derived instanceof DerivedArray, "derived array fallback");
function AlternateTarget() {}
AlternateTarget.prototype = {alternate: 7};
const alternate = Reflect.construct(SavedArray, [], AlternateTarget);
check(SavedObject.getPrototypeOf(alternate) === AlternateTarget.prototype, "newtarget fallback");
const bound = SavedArray.bind(null, 2);
check(new bound().length === 2, "bound fallback");
let proxyCalls = 0;
const proxy = new Proxy(SavedArray, {construct(target, args, newTarget) { proxyCalls++; return Reflect.construct(target, args, newTarget); }});
check(new proxy(2).length === 2 && proxyCalls === 1, "proxy fallback");

const retained = {tag: 7}, sink = new SavedArray(16);
function allocatingAlias(fn, value, index) {
  const result = new fn();
  result[0] = value;
  sink[index & 15] = {value, result, index};
  return result[0] === value && value.tag === 7;
}
let live = 0;
for (let i = 0; i < 12000; i++) live += allocatingAlias(SavedArray, retained, i) ? 1 : 0;
check(live === 12000 && retained.tag === 7, "held callable and arguments under moving gc");
function nested(fn, arg) { return leaf(fn, arg); }
function leaf(fn, arg) { return new fn(arg); }
for (let i = 0; i < 6000; i++) nested(SavedArray, 2);
argumentEffects = 0;
check(nested(Replacement, effect(2)).tag === 7 && argumentEffects === 1, "nested identity miss without replay");
check(nested(SavedArray, 2).length === 2, "nested recovered source operation");
console.log(JSON.stringify({checks, live, proxyCalls, staticCalls}));
