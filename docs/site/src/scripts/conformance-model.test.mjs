import { test } from "node:test";
import assert from "node:assert/strict";
import { conformanceModel } from "./conformance-model.mjs";

function fixture() {
  const counts = { total: 2, passed: 1, failed: 0, skipped: 1, crashed: 0, timed_out: 0, oom: 0 };
  return { test262_commit: "corpus", engine_commit: "engine", ran_at: "now",
    runner: { executable_sha256: "0".repeat(64), target: "synthetic", debug_assertions: true, semantic_config: {timeout_ms:5000,max_heap_bytes:1024,jit_tier:"interpreter",snapshot_isolates:false,skip_features:[],skip_flags:[],ignored_tests:[],known_panics:[],engine_environment:{}} },
    totals: { ...counts }, by_section: { "language/x/y": { ...counts } },
    tests: [{ path: "language/x/y/a.js", esid: null, features: [], wall_ms: 1, outcome: { kind: "pass" } },
      { path: "language/x/y/b.js", esid: null, features: [], wall_ms: 2, outcome: { kind: "skipped", reason: { kind: "missing_frontmatter" } } }] };
}
test("canonical passes and typed skips retain identity with derived counts", () => {
  assert.equal(conformanceModel("test262", fixture()).totals.skipped, 1);
});
test("duplicate paths, unknown skip and wrong rollups fail", () => {
  for (const mutate of [data => { data.tests[1].path = data.tests[0].path; },
    data => { data.totals.passed++; }, data => { data.by_section["language/x/y"].skipped--; },
    data => { data.tests[1].outcome = { kind: "skipped", feature: "old" }; }]) {
    const data = fixture(); mutate(data);
    assert.throws(() => conformanceModel("test262", data));
  }
});
test("Node.js remains distinct and aggregate-only Test262 is rejected", () => {
  const data = fixture(); delete data.tests; delete data.runner; data.failing_tests = [];
  assert.throws(() => conformanceModel("test262", data));
  assert.equal(conformanceModel("nodejs", data).totals.total, 2);
});
test("full failure stack is preserved in the derived display view", () => {
  const data = fixture(); data.tests[0].outcome = { kind: "fail", reason: "message", stack: "full\nstack" };
  for (const counts of [data.totals, data.by_section["language/x/y"]]) { counts.passed--; counts.failed++; }
  assert.equal(conformanceModel("test262", data).failures[0].stack, "full\nstack");
});

test("nullable keys, source-size bounds and runner policy fields are required", () => {
  for (const mutate of [data => { delete data.tests[0].esid; },
    data => { data.tests[0].outcome = {kind:"fail",reason:"detail"}; },
    data => { data.tests[1].outcome.reason = {kind:"source_too_large",bytes:42,limit:42}; },
    data => { data.runner.target = ""; }, data => { data.runner.extra = "unknown"; },
    data => { data.runner.semantic_config.timeout_ms = 30001; }]) {
    const data=fixture(); mutate(data);
    assert.throws(() => conformanceModel("test262",data));
  }
});
