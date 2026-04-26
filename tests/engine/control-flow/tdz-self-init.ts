/* otter-test:
name = "control-flow: TDZ on self-init throws ReferenceError"
[expect]
exit_code = 1
*/
let a = a + 1;
