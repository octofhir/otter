// `otter -p` prints the completion value after the event loop drains, so the
// value must stay rooted while promise jobs allocate under GC stress.
function run() {
  Promise.resolve().then(() => {
    const churn = [];
    for (let i = 0; i < 200; i++) churn.push({ i });
    console.log("then " + churn.length);
  });
  return "done:" + [1, 2].length;
}
run();
