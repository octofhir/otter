/* otter-test:
name = "control-flow: with statement is rejected at compile time"
[expect]
exit_code = 1
*/
let o = { x: 1 };
with (o) {
  x = 2;
}
