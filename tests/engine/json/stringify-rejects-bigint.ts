/* otter-test:
name = "json: stringify of a BigInt aborts with TypeMismatch"
[expect]
exit_code = 1
*/
// Spec §25.5.2.4: a BigInt anywhere in the graph triggers a
// TypeError. The foundation surfaces this as a host-level
// TYPE_MISMATCH (catchable JS errors arrive once we ship the full
// Error hierarchy).
JSON.stringify({ big: 1n });
