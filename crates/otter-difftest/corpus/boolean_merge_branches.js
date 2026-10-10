// Conditions built from `&&`, `||`, `!`, comma and ?: in if/while/do/for
// tests and in returned values an inlined caller branches on: merged
// booleans, constant answers from inlined methods, operands with effects,
// non-boolean operands whose truthiness decides, and a receiver whose shape
// changes mid-loop.
const HELD = 4, SUSPENDED = 2, RUNNABLE = 1;

class Task {
  constructor(state) { this.state = state; this.count = 0; }
  isHeldOrSuspended() { return (this.state & HELD) != 0 || this.state == SUSPENDED; }
  isRunnable() { return !((this.state & HELD) != 0) && (this.state & RUNNABLE) != 0; }
  always() { return true; }
  never() { return false; }
  either(a, b) { return a ? this.always() : b && this.never(); }
}

function schedule(tasks, n) {
  let held = 0, run = 0, other = 0;
  for (let i = 0; i < n; i++) {
    const t = tasks[i % tasks.length];
    if (t.isHeldOrSuspended()) held++;
    else if (t.isRunnable() && t.always()) run++;
    else other++;
    if (t.never() || t.either(i & 1, i & 2)) t.count++;
    if (!(t.isRunnable() || t.never())) t.count += 2;
  }
  return held + ":" + run + ":" + other + ":" + tasks.map((t) => t.count).join("/");
}

const out = [];
const tasks = [new Task(0), new Task(HELD), new Task(SUSPENDED), new Task(RUNNABLE), new Task(HELD | RUNNABLE)];
out.push(schedule(tasks, 30000));

// The state field turns into other values mid-run; truthiness of
// non-boolean operands still decides `||` and `&&` in conditions.
tasks[3].state = "x";
tasks[0].state = undefined;
out.push(schedule(tasks, 3000));

// Effects in operands run exactly when short-circuiting allows.
let log = 0;
const hit = (v, bit) => { log = (log * 3 + bit) % 1000003; return v; };
function effects(n) {
  let s = 0;
  for (let i = 0; i < n; i++) {
    if (hit(i & 1, 1) || hit(i & 2, 2) && !hit(i & 4, 0)) s++;
    while (hit(s > i, 1) && (hit(0, 2), s-- > 0)) {}
    let k = 0;
    do { k++; } while (k < 3 && (hit(k, 1), !hit(i % 5 == 0, 2)));
    for (let j = 0; j < 4 && (i & 1 || j < 2); j++) s += j;
    s += (hit(i % 3, 1) && hit(i % 7, 2)) ? 1 : 2;
  }
  return s + ":" + log;
}
out.push(effects(20000));

// Merged values that are not booleans keep their identity when read as
// values rather than tested.
function values(n) {
  let objects = 0, strings = 0;
  for (let i = 0; i < n; i++) {
    const v = (i & 1 && { k: i }) || (i & 2 && "s" + i) || null;
    if (v && typeof v === "object") objects += v.k & 3;
    else if (v) strings += v.length;
  }
  return objects + ":" + strings;
}
out.push(values(20000));
console.log(out.join("\n"));
