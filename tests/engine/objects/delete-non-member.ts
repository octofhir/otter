/* otter-test:
name = "object: delete on non-member returns true; void discards value"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
// Per spec, `delete <non-Reference>` returns true.
if ((delete (1 + 2)) !== true) fail();
if ((delete "hello") !== true) fail();
// `void expr` always evaluates to undefined.
if (void 0 !== undefined) fail();
let counter = 0;
let r = void (counter = counter + 1);
if (r !== undefined) fail();
if (counter !== 1) fail();
// `delete` on a missing own property still returns true.
let o: any = {};
if ((delete o.missing) !== true) fail();
