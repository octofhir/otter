/* otter-test:
name = "reflect: full §28.1 static surface"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// get / set / has / deleteProperty / ownKeys.
const o = { a: 1, b: 2 };
if (Reflect.get(o, "a") !== 1) fail();
if (!Reflect.has(o, "b")) fail();
if (Reflect.has(o, "c")) fail();
Reflect.set(o, "c", 3);
if (o.c !== 3) fail();
if (!Reflect.deleteProperty(o, "a")) fail();
if (Reflect.has(o, "a")) fail();
const keys = Reflect.ownKeys(o);
if (keys.length !== 2) fail();

// getPrototypeOf / setPrototypeOf.
const proto = { x: 99 };
const child = {};
Reflect.setPrototypeOf(child, proto);
if (Reflect.getPrototypeOf(child) !== proto) fail();

// isExtensible / preventExtensions.
const ext = {};
if (!Reflect.isExtensible(ext)) fail();
Reflect.preventExtensions(ext);
if (Reflect.isExtensible(ext)) fail();

// defineProperty + getOwnPropertyDescriptor.
const obj = {};
Reflect.defineProperty(obj, "y", {
    value: 5,
    writable: true,
    enumerable: true,
    configurable: true,
});
const desc = Reflect.getOwnPropertyDescriptor(obj, "y");
if (desc.value !== 5 || !desc.writable) fail();
if (!desc.enumerable || !desc.configurable) fail();
if (Reflect.getOwnPropertyDescriptor(obj, "missing") !== undefined) fail();
