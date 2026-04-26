/* otter-test:
name = "object: Object.getPrototypeOf returns parent"
[expect]
exit_code = 0
*/
let p = { x: 1 };
let o = Object.create(p);
Object.getPrototypeOf(o).x;
