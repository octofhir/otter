/* otter-test:
name = "control-flow: switch dispatches by strict equality"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
function classify(n: number): string {
  switch (n) {
    case 0:
      return "zero";
    case 1:
      return "one";
    case 2:
      return "two";
    default:
      return "other";
  }
}
if (classify(0) !== "zero") fail();
if (classify(1) !== "one") fail();
if (classify(2) !== "two") fail();
if (classify(7) !== "other") fail();
// Strict equality: "1" must NOT match case 1.
if (classify(("1" as unknown) as number) !== "other") fail();
