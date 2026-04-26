/* otter-test:
name = "ts: type alias erases to nothing"
[expect]
exit_code = 0
*/
type Foo = number;
type Bar<T> = { value: T };
undefined;
