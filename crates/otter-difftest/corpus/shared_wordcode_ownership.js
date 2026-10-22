// Shared wordcode and source ownership survive later CodeBlock construction,
// moving collections and prototype/descriptor changes after native admission.
"use strict";
var calls = 0;
function Box(x) { this.x = x; }
Box.prototype.bump = function (n) { calls++; this.x += n; return this.x; };
var boxes = [new Box(0), new Box(100), new Box(200)];
function hot(o, i) {
  if (!("x" in o)) throw new Error("missing x");
  return o.bump(i & 3) + o.x;
}
var hash = 0;
for (var i = 0; i < 12000; i++) hash = (hash + hot(boxes[i % 3], i)) | 0;
var functions = [];
var functionSum = 0;
for (var j = 0; j < 32; j++) {
  var f = new Function("o", "return o.x + " + j + ";");
  functions.push(f);
  for (var k = 0; k < 64; k++) functionSum += f(boxes[k % 3]);
  hash = (hash + hot(boxes[j % 3], j)) | 0;
  if (typeof gc === "function" && j % 4 === 0) gc();
}
for (var late = 0; late < 12000; late++) {
  hash = (hash + hot(boxes[late % 3], late)) | 0;
  if ((late & 127) === 0) {
    var garbage = [{ x: late }, { y: late + 1 }];
    if (garbage[0].x + 1 !== garbage[1].y) throw new Error("literal site");
  }
}
Box.prototype.bump = function (n) { calls++; this.x += n + 1; return this.x; };
for (var changed = 0; changed < 1000; changed++) hash = (hash + hot(boxes[changed % 3], changed)) | 0;
var value = boxes[0].x;
var reads = 0;
Object.defineProperty(boxes[0], "x", {
  get: function () { reads++; return value; },
  set: function (x) { value = x; },
  configurable: true,
});
for (var accessor = 0; accessor < 100; accessor++) hash = (hash + hot(boxes[0], accessor)) | 0;
if (reads !== 300) throw new Error("accessor replay: " + reads);
if (calls !== 25132) throw new Error("call replay: " + calls);
if (typeof gc === "function") gc();
for (var kept = 0; kept < functions.length; kept++) functionSum += functions[kept](boxes[kept % 3]);
console.log("shared-wordcode", hash, calls, reads, functionSum);
if (String(functions[31]).indexOf("o.x + 31") === -1) throw new Error("lost function source");
