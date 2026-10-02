function make() { return new Error("x"); }
function hot(n) {
  let acc = 0;
  for (let i = 0; i < n; i++) acc += make().message.length;
  return acc;
}
console.log(hot(Number(process.argv[2])));
console.log(hot(Number(process.argv[3])));
