/* otter-test:
name = "object: Object.create(null) detaches the chain"
[expect]
exit_code = 0
*/
let o = Object.create(null);
let p = Object.getPrototypeOf(o);
p;
