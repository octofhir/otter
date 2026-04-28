/* otter-test:
name = "symbols: arr[Symbol.iterator]() returns an iterator-shaped value"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const arr = [1, 2, 3];
const factory = arr[Symbol.iterator];
if (factory === undefined) fail();
const iter = arr[Symbol.iterator]();
if (iter === undefined) fail();
// Symbol.iterator is the same singleton across reads.
if (Symbol.iterator !== Symbol.iterator) fail();
