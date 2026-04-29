/* otter-test:
name = "iterators: for-of consults user-defined [Symbol.iterator]"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// Counts from 0 to limit-1 via the spec iterator protocol.
function counter(limit) {
    const obj = {};
    obj[Symbol.iterator] = function () {
        let i = 0;
        const iter = {};
        iter.next = function () {
            const result = {};
            if (i < limit) {
                result.value = i;
                result.done = false;
                i = i + 1;
            } else {
                result.value = undefined;
                result.done = true;
            }
            return result;
        };
        return iter;
    };
    return obj;
}

let total = 0;
for (const n of counter(4)) {
    total = total + n;
}
if (total !== 6) fail();

// Re-iterating yields a fresh sequence (each [Symbol.iterator]()
// call returns a brand-new iterator).
let count = 0;
const it = counter(3);
for (const _ of it) count = count + 1;
for (const _ of it) count = count + 1;
if (count !== 6) fail();
