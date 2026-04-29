/* otter-test:
name = "object: Object.hasOwn + hasOwnProperty + isPrototypeOf cover own/inherited probes"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let parent = { inherited: 1 };
let child = Object.create(parent);
child.own = 2;
// Object.hasOwn ignores inherited keys.
if (Object.hasOwn(child, "own") !== true) fail();
if (Object.hasOwn(child, "inherited") !== false) fail();
// Object.prototype.hasOwnProperty matches.
if (child.hasOwnProperty("own") !== true) fail();
if (child.hasOwnProperty("inherited") !== false) fail();
// isPrototypeOf — `parent` is in `child`'s chain.
if (parent.isPrototypeOf(child) !== true) fail();
if (child.isPrototypeOf(parent) !== false) fail();
// propertyIsEnumerable — true for own enumerable, false otherwise.
if (child.propertyIsEnumerable("own") !== true) fail();
if (child.propertyIsEnumerable("inherited") !== false) fail();
