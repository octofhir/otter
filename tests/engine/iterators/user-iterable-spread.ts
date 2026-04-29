/* otter-test:
name = "iterators: array spread consults user [Symbol.iterator]"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

function range(start, stop) {
    const obj = {};
    obj[Symbol.iterator] = function () {
        let cur = start;
        const iter = {};
        iter.next = function () {
            const r = {};
            if (cur < stop) {
                r.value = cur;
                r.done = false;
                cur = cur + 1;
            } else {
                r.value = undefined;
                r.done = true;
            }
            return r;
        };
        return iter;
    };
    return obj;
}

const collected = [...range(2, 5)];
if (collected.length !== 3) fail();
if (collected[0] !== 2) fail();
if (collected[1] !== 3) fail();
if (collected[2] !== 4) fail();

// Inline mix with literals.
const wider = [0, ...range(1, 3), 99];
if (wider.length !== 4) fail();
if (wider[0] !== 0) fail();
if (wider[1] !== 1) fail();
if (wider[2] !== 2) fail();
if (wider[3] !== 99) fail();
