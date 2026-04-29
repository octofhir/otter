/* otter-test:
name = "abstract-ops: ToPrimitive consults [Symbol.toPrimitive]"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

const obj = {};
obj[Symbol.toPrimitive] = function (hint) {
    if (hint === "number") return 42;
    if (hint === "string") return "hello";
    return "default-hint";
};

// Binary `+` passes the "default" hint to [Symbol.toPrimitive].
const plus = obj + "";
if (plus !== "default-hint") fail();

// `obj + 1` hits the numeric arm because both ToPrimitive(default)
// results are `"default-hint"` (string) and `1` (number) — the
// post-coercion ApplyStringOrNumericBinaryOperator path picks
// string concat.
const concat = obj + 1;
if (concat !== "default-hint1") fail();
