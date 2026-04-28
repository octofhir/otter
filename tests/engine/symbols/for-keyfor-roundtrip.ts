/* otter-test:
name = "symbols: Symbol.for / Symbol.keyFor round-trip"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const a = Symbol.for("kk");
const b = Symbol.for("kk");
if (a !== b) fail();
if (Symbol.keyFor(a) !== "kk") fail();
const local = Symbol("kk");
if (Symbol.keyFor(local) !== undefined) fail();
