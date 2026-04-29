/* otter-test:
name = "control-flow: switch falls through missing breaks"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let trace: string[] = [];
function tag(n: number): void {
  switch (n) {
    case 1:
      trace.push("one");
    // fallthrough
    case 2:
      trace.push("two");
      break;
    case 3:
      trace.push("three");
      break;
  }
}
tag(1);
tag(2);
tag(3);
if (trace.length !== 4) fail();
if (trace[0] !== "one") fail();
if (trace[1] !== "two") fail();
if (trace[2] !== "two") fail();
if (trace[3] !== "three") fail();
