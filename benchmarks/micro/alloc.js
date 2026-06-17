// Object-model allocation ceiling: 3M two-field literals.
// otter ~1733ms / 1557MB / 24 GC cycles vs node ~130ms compute (~13x).
// ~519 bytes/object: ObjectBody god-struct (object.rs:389, ~25 fields).
function work(){let s=0; for(let i=0;i<3000000;i++){const o={a:i,b:i+1}; s+=o.a+o.b;} return s;}
console.log(work());
