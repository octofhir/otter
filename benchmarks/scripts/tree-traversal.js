// Object graph traversal: binary tree build + recursive and iterative walks.
const DEPTH = 13;
const ROUNDS = 180;

function makeNode(depth, seed) {
  if (depth === 0) return null;
  const value = ((seed * 1103515245 + 12345) >>> 0) & 2047;
  return {
    value,
    tag: depth,
    left: makeNode(depth - 1, (seed * 2 + 1) | 0),
    right: makeNode(depth - 1, (seed * 2 + 2) | 0),
  };
}

function recursiveSum(node, salt) {
  if (node === null) return 0;
  node.tag = (node.tag + salt) & 255;
  return (
    ((node.value + node.tag) ^ salt) +
    recursiveSum(node.left, salt) +
    recursiveSum(node.right, salt)
  );
}

function iterativeSum(root, salt) {
  const stack = [root];
  let acc = 0;
  while (stack.length > 0) {
    const node = stack.pop();
    if (node === null) continue;
    acc = (acc + node.value + node.tag + salt) | 0;
    if (node.left !== null) stack.push(node.left);
    if (node.right !== null) stack.push(node.right);
  }
  return acc;
}

const root = makeNode(DEPTH, 1);
let acc = 0;
for (let r = 0; r < ROUNDS; r++) {
  acc = (acc + recursiveSum(root, r & 31)) | 0;
  acc = (acc ^ iterativeSum(root, (r * 3) & 31)) | 0;
}

console.log(acc);
