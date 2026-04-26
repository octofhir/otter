/* otter-test:
name = "object: Object.setPrototypeOf rewires the chain"
[expect]
exit_code = 0
*/
let p = { greeting: "hello" };
let o = {};
Object.setPrototypeOf(o, p);
o.greeting;
