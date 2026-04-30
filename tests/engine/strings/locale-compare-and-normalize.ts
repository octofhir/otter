/* otter-test:
name = "string: localeCompare / normalize / lastIndexOf / toString"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
// localeCompare — sign of the relative order.
if ("a".localeCompare("b") >= 0) fail();
if ("b".localeCompare("a") <= 0) fail();
if ("a".localeCompare("a") !== 0) fail();
// normalize — accepts the four canonical forms; default NFC.
if ("hello".normalize() !== "hello") fail();
if ("hello".normalize("NFC") !== "hello") fail();
if ("hello".normalize("NFD") !== "hello") fail();
// lastIndexOf — searches backwards.
if ("hello".lastIndexOf("l") !== 3) fail();
if ("aaaa".lastIndexOf("a") !== 3) fail();
if ("hello".lastIndexOf("x") !== -1) fail();
// toString / valueOf — return the receiver.
if ("abc".toString() !== "abc") fail();
if ("abc".valueOf() !== "abc") fail();
