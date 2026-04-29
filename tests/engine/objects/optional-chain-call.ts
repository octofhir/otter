/* otter-test:
name = "object: optional chaining handles callable absence"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let api: any = {
  greet(name: string): string {
    return "hi, " + name;
  },
};
let r1 = api?.greet?.("world");
if (r1 !== "hi, world") fail();
let r2 = api?.missing?.("oops");
if (r2 !== undefined) fail();
let f: any = null;
let r3 = f?.();
if (r3 !== undefined) fail();
