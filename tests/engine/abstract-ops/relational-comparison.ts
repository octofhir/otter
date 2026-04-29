/* otter-test:
name = "abstract-ops: <, <=, >, >= cover ECMA-262 §7.2.14"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// Number on both sides.
if (!(1 < 2)) fail();
if (1 > 2) fail();
if (!(2 >= 2)) fail();
if (!(2 <= 2)) fail();

// NaN cascades: every relational op returns false.
const nan = 0 / 0;
if (nan < 1) fail();
if (nan > 1) fail();
if (nan <= 1) fail();
if (nan >= 1) fail();
if (1 < nan) fail();
if (1 > nan) fail();

// String on both sides — lexicographic.
if (!("abc" < "abd")) fail();
if ("abc" >= "abd") fail();
if (!("a" < "b")) fail();

// String × Number — string parses; "1" < 2 is true.
if (!("1" < 2)) fail();
if (!("0" < 1)) fail();
if ("foo" < 1) fail();
if ("foo" > 1) fail();

// Boolean coercion.
if (!(true > false)) fail();
if (!(true >= 1)) fail();

// `null` → 0, `undefined` → NaN. Cascading false.
if (!(null < 1)) fail();
if (undefined < 1) fail();
if (undefined > 1) fail();

// ToPrimitive(number) on objects via valueOf.
const obj = {};
obj.valueOf = function () {
    return 5;
};
if (!(obj < 6)) fail();
if (!(4 < obj)) fail();
if (obj < 5) fail();
if (!(obj <= 5)) fail();
