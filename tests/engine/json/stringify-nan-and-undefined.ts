/* otter-test:
name = "json: stringify NaN/Infinity → null, undefined → omitted"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
// NaN and ±Infinity become null per spec §25.5.2.4.
if (JSON.stringify({ x: NaN }) !== "{\"x\":null}") fail();
if (JSON.stringify({ x: Infinity }) !== "{\"x\":null}") fail();
if (JSON.stringify({ x: -Infinity }) !== "{\"x\":null}") fail();
// undefined inside an object is dropped.
if (JSON.stringify({ a: 1, b: undefined, c: 2 }) !== "{\"a\":1,\"c\":2}") fail();
// undefined inside an array becomes null.
if (JSON.stringify([1, undefined, 3]) !== "[1,null,3]") fail();
