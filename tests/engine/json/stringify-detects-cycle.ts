/* otter-test:
name = "json: stringify of a cyclic graph aborts with TypeMismatch"
[expect]
exit_code = 1
*/
// Spec §25.5.2.4: cyclic structure → TypeError. Foundation
// surfaces it as a host-level TYPE_MISMATCH.
const a = { x: 1 };
a.self = a;
JSON.stringify(a);
