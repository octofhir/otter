/* otter-test:
name = "methods: calling a non-function property raises NotCallable"
[expect]
exit_code = 1
*/
const o = { notFn: 1 };
o.notFn();
