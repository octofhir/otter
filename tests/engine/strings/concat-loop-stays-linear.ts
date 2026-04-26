/* otter-test:
name = "string: long concat chain"
[expect]
exit_code = 0
*/
// Build a long cons rope statically — slice 12 will replace this
// with a real `s += piece` loop.
"a" + "b" + "c" + "d" + "e" + "f" + "g" + "h" + "i" + "j";
