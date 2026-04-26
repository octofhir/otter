/* otter-test:
name = "object: instanceof walks proto chain"
[expect]
exit_code = 0
*/
let parent = {};
let unrelated = {};
let child = Object.create(parent);
let yes = child instanceof parent;
let no = child instanceof unrelated;
yes;
no;
