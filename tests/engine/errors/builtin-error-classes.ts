/* otter-test:
name = "errors: seven canonical native error classes"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// `new TypeError("...")` produces an instance whose prototype
// chain passes through TypeError.prototype and Error.prototype.
let caught;
try {
    throw new TypeError("bad type");
} catch (e) {
    caught = e;
}
if (!(caught instanceof TypeError)) fail();
if (!(caught instanceof Error)) fail();
if (caught.name !== "TypeError") fail();
if (caught.message !== "bad type") fail();

// Each of the six canonical subclasses has the right name and
// keeps the `instanceof Error` invariant.
function check(thrown, ctorName) {
    if (thrown.name !== ctorName) fail();
    if (!(thrown instanceof Error)) fail();
}
try { throw new RangeError("r"); } catch (e) { check(e, "RangeError"); if (!(e instanceof RangeError)) fail(); }
try { throw new SyntaxError("s"); } catch (e) { check(e, "SyntaxError"); if (!(e instanceof SyntaxError)) fail(); }
try { throw new ReferenceError("ref"); } catch (e) { check(e, "ReferenceError"); if (!(e instanceof ReferenceError)) fail(); }
try { throw new URIError("uri"); } catch (e) { check(e, "URIError"); if (!(e instanceof URIError)) fail(); }
try { throw new EvalError("ev"); } catch (e) { check(e, "EvalError"); if (!(e instanceof EvalError)) fail(); }

// A bare `Error` instance is `instanceof Error` but not any
// subclass.
const base = new Error("plain");
if (!(base instanceof Error)) fail();
if (base instanceof TypeError) fail();
if (base.name !== "Error") fail();
if (base.message !== "plain") fail();

// Calling without `new` yields the same instance shape
// (§20.5.1.1 step 1 — error constructors accept both forms).
const noNew = TypeError("no-new");
if (!(noNew instanceof TypeError)) fail();
if (noNew.message !== "no-new") fail();

// Omitted message → inherited empty default from
// Error.prototype.message ("").
const noMsg = new RangeError();
if (noMsg.message !== "") fail();
