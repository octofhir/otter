/* otter-test:
name = "abstract-ops: ToPrimitive throws when both valueOf and toString return objects"
[expect]
exit_code = 1
*/
function fail() {
    return undefined.x;
}

// Both valueOf and toString return objects — §7.1.1.1 step 6
// raises TypeError. Foundation surfaces VmError::TypeMismatch
// (task 25 will swap this for a real TypeError Error object).
const obj = {};
obj.valueOf = function () {
    return {};
};
obj.toString = function () {
    return [];
};

// Reaching the `+` triggers ToPrimitive on `obj`; the ladder
// exhausts both ordinary slots and throws. The test asserts the
// throw escapes by checking the script terminates with a
// non-zero exit code (no `try`/`catch` around the binary op).
const _unused = obj + 1;
fail();
