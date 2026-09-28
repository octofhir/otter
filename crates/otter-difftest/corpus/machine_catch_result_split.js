// An optimized caller catches what its optimized callee throws. The call's
// result lives in one register on the normal path and is moved to another
// before the block terminator because the catch block also receives it; the
// exceptional edge must perform the same move, or the catch reads whatever
// the target register held (the caller's own closure).
var token = { t: 1 };
function thrower(x) { if (x < 0) throw token; return x + 1; }
function catcher(x) {
  try { return thrower(x); } catch (e) { return e === token ? 1000 : -1; }
}
var warm = 0;
for (var i = 0; i < 56000; i++) warm += catcher(i);
var caught = 0;
for (var j = 0; j < 512; j++) caught += catcher(-1);
console.log("machine_catch_result_split", warm, caught);
