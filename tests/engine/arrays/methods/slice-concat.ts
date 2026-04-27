/* otter-test:
name = "array method: slice/concat"
[expect]
exit_code = 0
*/
let a = [1, 2, 3, 4, 5];
a.slice(1, 4).join("-");
a.concat([6, 7]).length;
