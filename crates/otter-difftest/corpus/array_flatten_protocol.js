"use strict";
// Flattening preserves live protocol order, source/target roots and UTF-16.
function checkFlatten(condition, message) {
    if (!condition) throw new Error(message);
}
for (const method of ["flat", "flatMap"]) {
    const events = [], marker = {}, leaf = {text: "\ud800flat\udc00"}, tail = {};
    const values = {1: null, 2: tail}, innerValues = {1: leaf, 2: tail};
    let length = Number.MAX_SAFE_INTEGER;
    const inner = new Proxy([], {
        get(target, key) {
            events.push("inner-get:" + key);
            return key === "length" ? 3 : innerValues[key];
        },
        has(target, key) { events.push("inner-has:" + key); return key in innerValues; },
        ownKeys() { throw new Error("inner enumeration"); },
        getPrototypeOf() { throw new Error("inner prototype walk"); }
    });
    values[1] = inner;
    const writes = [];
    const output = new Proxy({}, {
        defineProperty(target, key, descriptor) {
            events.push("define:" + key);
            checkFlatten(descriptor.writable && descriptor.enumerable && descriptor.configurable,
                "CreateDataProperty descriptors");
            writes.push(descriptor.value);
            if (key === "0") {
                delete innerValues[2];
                delete values[2];
                values[3] = tail;
                length = 0;
            }
            for (let n = 0; n < 8; n++) globalThis.flattenChurn = {n, child: {n}};
            return Reflect.defineProperty(target, key, descriptor);
        }
    });
    const ctor = {get [Symbol.species]() {
        events.push("species");
        return function(count) { events.push("create:" + count); return output; };
    }};
    const receiver = new Proxy([], {
        get(target, key) {
            if (key === "length") { events.push("length"); return length; }
            if (key === "constructor") { events.push("constructor"); return ctor; }
            events.push("get:" + key);
            return values[key];
        },
        has(target, key) {
            events.push("has:" + key);
            if (key === "4") throw marker;
            return key in values;
        },
        ownKeys() { throw new Error("source enumeration"); },
        getPrototypeOf() { throw new Error("source prototype walk"); }
    });
    const thisArg = {};
    const mapper = function(value, index, source) {
        checkFlatten(this === thisArg && source === receiver && arguments.length === 3,
            "mapper receiver and arguments");
        events.push("map:" + index);
        return value;
    };
    let caught;
    try { Array.prototype[method].call(receiver, method === "flat" ? 1 : mapper, thisArg); }
    catch (error) { caught = error; }
    checkFlatten(caught === marker, "exact abrupt completion");
    const expected = ["length", "constructor", "species", "create:0", "has:0", "has:1", "get:1"];
    if (method === "flatMap") expected.push("map:1");
    expected.push("inner-get:length", "inner-has:0", "inner-has:1", "inner-get:1", "define:0",
        "inner-has:2", "has:2", "has:3", "get:3");
    if (method === "flatMap") expected.push("map:3");
    expected.push("define:1", "has:4");
    checkFlatten(events.join("|") === expected.join("|"), "ordered live flatten: " + method);
    checkFlatten(writes.length === 2 && writes[0] === leaf && writes[1] === tail,
        "current target and leaf identities");
    checkFlatten(output[0].text.charCodeAt(0) === 0xd800
        && output[0].text.charCodeAt(5) === 0xdc00, "exact UTF-16 units");
}
for (const method of ["flat", "flatMap"]) {
    const marker = {}, events = [];
    const inner = new Proxy([], {
        get(target, key) { events.push("get:" + key); return key === "length" ? Number.MAX_SAFE_INTEGER : 71; },
        has(target, key) { events.push("has:" + key); if (key === "2") throw marker; return key === "1"; }
    });
    let caught;
    try { Array.prototype[method].call([inner], method === "flat" ? 1 : value => value); }
    catch (error) { caught = error; }
    checkFlatten(caught === marker && events.join("|") === "get:length|has:0|has:1|get:1|has:2",
        "nested full ToLength protocol");
}
const leaf = {marker: 719};
let nested = leaf;
for (let n = 0; n < 32; n++) nested = [nested];
checkFlatten([nested, nested, nested].flat(Infinity).every(value => value === leaf),
    "depth-first frames and sibling reuse");
const revoked = Proxy.revocable([], {}); revoked.revoke();
checkFlatten([revoked.proxy].flat(0)[0] === revoked.proxy, "depth zero skips IsArray");
let caught;
try { [revoked.proxy].flat(1); } catch (error) { caught = error; }
checkFlatten(caught instanceof TypeError, "positive depth checks revoked IsArray");
const arrayLike = {[Symbol.isConcatSpreadable]: true, length: 1, 0: leaf};
const innerArray = [leaf];
const shallow = [1, 2].flatMap(value => value === 1 ? [innerArray] : arrayLike);
checkFlatten(shallow.length === 2 && shallow[0] === innerArray && shallow[1] === arrayLike,
    "only IsArray flattens, and only one mapper level");
const inherited = Object.create({get 0() {
    checkFlatten(this === inherited, "inherited getter receiver"); return leaf;
}});
inherited.length = 1;
checkFlatten(Array.prototype.flat.call(inherited)[0] === leaf, "inherited source index");
const lengthMarker = {};
caught = undefined;
try { Array.prototype.flatMap.call({get length() { throw lengthMarker; }}, null); }
catch (error) { caught = error; }
checkFlatten(caught === lengthMarker, "length error before mapper validation");
console.log("ok:flatten-protocol");
"ok:flatten-protocol";
