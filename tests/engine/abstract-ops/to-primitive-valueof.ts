/* otter-test:
name = "abstract-ops: ToPrimitive(default) consults valueOf chain"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// User-defined valueOf returning a Number — the `default` hint
// drives `valueOf` first per §7.1.1.1.
const nObj = {};
nObj.valueOf = function () {
    return 7;
};
const sum = nObj + 1;
if (sum !== 8) fail();

// `+` between two valueOf-bearing objects — both coerce to
// numbers, then numeric add fires.
const a = {};
a.valueOf = function () {
    return 3;
};
const b = {};
b.valueOf = function () {
    return 4;
};
if (a + b !== 7) fail();
