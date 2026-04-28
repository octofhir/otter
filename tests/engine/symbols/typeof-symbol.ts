/* otter-test:
name = "symbols: typeof returns 'symbol' for Symbol values"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
if (typeof Symbol() !== "symbol") fail();
if (typeof Symbol.iterator !== "symbol") fail();
if (typeof Symbol.for("k") !== "symbol") fail();
// Typeof is not affected by the host's other primitives.
if (typeof "x" !== "string") fail();
if (typeof 1 !== "number") fail();
if (typeof undefined !== "undefined") fail();
if (typeof null !== "object") fail();
