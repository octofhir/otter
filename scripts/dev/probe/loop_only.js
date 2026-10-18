"use strict";
let sum = 0;
function loopOnly() { let s = 0; for (let i = 0; i < 1000; i++) s += i; sum += s; }
function props(obj) { let s = 0; for (let i = 0; i < 1000; i++) s += obj.x + obj.y; return s; }
loopOnly(); props({x:1,y:2});
