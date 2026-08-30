// A `return` inside a `try` whose region owns a `finally` must run that
// finally in every tier. Generated code returns through its own epilogue,
// so the return has to leave through an exact side exit instead.
const counts = { rest: 0, args: 0, plain: 0, nested: 0 };
const holder = {
  rest(...a) { try { return a[0]; } finally { counts.rest++; } },
  args(x) { try { return arguments[0]; } finally { counts.args++; } },
  plain(x) { try { return x; } finally { counts.plain++; } },
  nested(...a) {
    try {
      try { return a[0]; } finally { counts.nested++; }
    } finally { counts.nested++; }
  },
};
let sum = 0;
for (let i = 0; i < 4000; i++) {
  sum += ((x) => holder.rest(x))(i);
  sum += ((x) => holder.args(x))(i);
  sum += ((x) => holder.plain(x))(i);
  sum += ((x) => holder.nested(x))(i);
}
JSON.stringify({ sum, counts });
