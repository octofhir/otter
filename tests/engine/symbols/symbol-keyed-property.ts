/* otter-test:
name = "symbols: symbol-keyed object properties round-trip"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const sym = Symbol("k");
const obj = {};
obj[sym] = 1;
if (obj[sym] !== 1) fail();
// String key with the same descriptive name is distinct.
if (obj["Symbol(k)"] !== undefined) fail();
// Different symbol with the same description does not collide.
const other = Symbol("k");
if (obj[other] !== undefined) fail();
// Delete works.
const removed = delete obj[sym];
if (removed !== true) fail();
if (obj[sym] !== undefined) fail();
