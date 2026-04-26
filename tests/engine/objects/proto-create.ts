/* otter-test:
name = "object: Object.create + proto-chain lookup"
[expect]
exit_code = 0
*/
let parent = { x: 1 };
let child = Object.create(parent);
child.x;
