/* otter-test:
name = "symbols: Symbol.toStringTag well-known is a stable singleton"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
// Foundation does not yet ship Object.prototype.toString, but the
// well-known symbol must exist and be a stable singleton, and the
// runtime must accept it as a property key on user objects per
// ECMA-262 §22.1.10.
const tag1 = Symbol.toStringTag;
const tag2 = Symbol.toStringTag;
if (tag1 !== tag2) fail();
const obj = {};
obj[Symbol.toStringTag] = "Foo";
if (obj[Symbol.toStringTag] !== "Foo") fail();
