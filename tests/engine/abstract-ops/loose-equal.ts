/* otter-test:
name = "abstract-ops: == covers ECMA-262 §7.2.13 IsLooselyEqual"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// null == undefined — only cross-kind primitive pair that is
// loosely equal.
if (!(null == undefined)) fail();
if (!(undefined == null)) fail();
if (null == 0) fail();
if (undefined == 0) fail();

// Number x String — string parses through ToNumber.
if (!(1 == "1")) fail();
if (!("0" == 0)) fail();
if ("0" == "00") fail();

// Boolean → Number coercion, then recurse.
if (!(true == 1)) fail();
if (!(false == 0)) fail();
if (!("1" == true)) fail();

// NaN never equals NaN under ==.
const nan = 0 / 0;
if (nan == nan) fail();

// +0 == -0.
if (!(0 == -0)) fail();
if (!(-0 == 0)) fail();

// `!=` mirrors the negation.
if (1 != "1") fail();
if (null != undefined) fail();

// Symbols compare by identity even loosely.
const sym = Symbol("x");
const sym2 = Symbol("x");
if (!(sym == sym)) fail();
if (sym == sym2) fail();
