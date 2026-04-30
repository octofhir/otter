/* otter-test:
name = "generators: function* yields each value then reports done"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

function* numbers() {
    yield 1;
    yield 2;
    yield 3;
}

const it = numbers();
const a = it.next();
if (a.value !== 1 || a.done !== false) fail();
const b = it.next();
if (b.value !== 2 || b.done !== false) fail();
const c = it.next();
if (c.value !== 3) fail();
const d = it.next();
if (d.done !== true) fail();
if (d.value !== undefined) fail();

// Calling next on an exhausted generator stays done.
const e = it.next();
if (e.done !== true) fail();

// Bare yield with no argument yields undefined.
function* empties() {
    yield;
    yield;
}
const f = empties();
if (f.next().value !== undefined) fail();
if (f.next().value !== undefined) fail();
if (f.next().done !== true) fail();
