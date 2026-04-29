/* otter-test:
name = "iterators: for-of on non-iterable throws"
[expect]
exit_code = 1
*/
function fail() {
    return undefined.x;
}

// Plain object with no [Symbol.iterator] — §7.4.3 throws TypeError.
// Foundation surfaces VmError::TypeMismatch (task 25 will swap
// for a real TypeError Error object).
const obj = {};
for (const _ of obj) {
    fail();
}
fail();
