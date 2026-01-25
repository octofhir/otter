function f() { return {}; }
function g() { return 1; }
// This should throw ReferenceError at runtime in non-strict mode
// But currently fails at compile time
try {
    f() = g();
    print("Executed assignment");
} catch (e) {
    print("Caught: " + e.name + ": " + e.message);
}
