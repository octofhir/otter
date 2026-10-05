/**
 * Distinct corpus readers for the shared dashboard.
 * Test262 derives every view from canonical complete rows and validates rollups.
 * Node.js retains its independent aggregate report contract. No Test262 fallback.
 */
const keys = ["total", "passed", "failed", "skipped", "crashed", "timed_out", "oom"];
const buckets = { pass: "passed", fail: "failed", skipped: "skipped", crash: "crashed", timeout: "timed_out", out_of_memory: "oom" };
const empty = () => Object.fromEntries(keys.map(key => [key, 0]));
const section = path => path.split("/").slice(0, 3).join("/");
const object = value => value && typeof value === "object" && !Array.isArray(value);
const integer = value => Number.isSafeInteger(value) && value >= 0;
const check = (condition, detail) => { if (!condition) throw new Error(`Invalid conformance report: ${detail}`); };
const exactKeys = (value, required) => object(value) && Object.keys(value).sort().join("\0") === [...required].sort().join("\0");
function totalsEqual(actual, expected) {
  return exactKeys(actual, keys) && keys.every(key => integer(actual[key]) && actual[key] === expected[key]);
}
function skipReason(reason) {
  check(object(reason), "skip reason missing");
  switch (reason.kind) {
    case "feature": case "flag": {
      const field = reason.kind;
      check(exactKeys(reason, ["kind", field]) && typeof reason[field] === "string", "invalid skip token");
      break;
    }
    case "ignored": case "known_panic":
      check(exactKeys(reason, ["kind", "pattern"]) && typeof reason.pattern === "string", "invalid skip pattern"); break;
    case "source_too_large":
      check(exactKeys(reason, ["kind", "bytes", "limit"]) && integer(reason.bytes) && integer(reason.limit) && reason.bytes > reason.limit, "invalid source bound"); break;
    case "missing_frontmatter": case "no_strictness_variant":
      check(exactKeys(reason, ["kind"]), "invalid unit skip reason"); break;
    default: throw new Error("Invalid conformance report: unknown skip reason");
  }
}
function outcomeDetail(outcome) {
  if (outcome.kind === "fail") return outcome.reason;
  if (outcome.kind === "crash") return outcome.panic;
  if (outcome.kind === "timeout") return `timeout after ${outcome.ms} ms`;
  return `oom: ${outcome.bytes} bytes requested`;
}

/** Validate the current Test262 contract and derive a display-only failure view.
 * @param {any} data
 */
export function test262Model(data) {
  check(exactKeys(data, ["test262_commit", "engine_commit", "ran_at", "runner", "totals", "by_section", "tests"]), "canonical Test262 fields required");
  check(typeof data.test262_commit === "string" && typeof data.engine_commit === "string" && typeof data.ran_at === "string", "capture identity");
  check(exactKeys(data.runner, ["executable_sha256", "target", "debug_assertions", "semantic_config"]) && typeof data.runner.executable_sha256 === "string" && /^[0-9a-f]{64}$/.test(data.runner.executable_sha256) && typeof data.runner.target === "string" && data.runner.target.length > 0 && typeof data.runner.debug_assertions === "boolean" && object(data.runner.semantic_config), "actual runner identity");
  const config = data.runner.semantic_config;
  check(exactKeys(config, ["timeout_ms", "max_heap_bytes", "jit_tier", "snapshot_isolates", "skip_features", "skip_flags", "ignored_tests", "known_panics", "engine_environment"]), "effective policy fields");
  check(integer(config.timeout_ms) && config.timeout_ms <= 30000 && integer(config.max_heap_bytes) && ["production-tiered", "template", "interpreter"].includes(config.jit_tier) && typeof config.snapshot_isolates === "boolean", "effective execution limits");
  check(["skip_features", "skip_flags", "ignored_tests", "known_panics"].every(key => Array.isArray(config[key]) && config[key].every(value => typeof value === "string")), "ordered skip policy");
  check(object(config.engine_environment) && Object.entries(config.engine_environment).every(([key, digest]) => ["OTTER_GC_STRESS", "OTTER_GC_VERIFY"].includes(key) && typeof digest === "string" && /^[0-9a-f]{64}$/.test(digest)), "safe GC control identity");
  check(Array.isArray(data.tests), "test rows required");
  const totals = empty(), sections = Object.create(null), failures = [];
  let previous = "";
  for (const row of data.tests) {
    check(exactKeys(row, ["path", "esid", "features", "outcome", "wall_ms"]), "test row fields");
    check(typeof row.path === "string" && row.path.endsWith(".js") && !row.path.endsWith("_FIXTURE.js") && !/[\\\0]/.test(row.path) && row.path.split("/").every(part => part && part !== "." && part !== ".."), "canonical test path");
    check(row.path > previous, "duplicate or unsorted test path"); previous = row.path;
    check((row.esid === null || typeof row.esid === "string") && Array.isArray(row.features) && row.features.every(feature => typeof feature === "string") && integer(row.wall_ms), "test metadata");
    const outcome = row.outcome;
    check(object(outcome) && Object.hasOwn(buckets, outcome.kind), "unknown outcome");
    switch (outcome.kind) {
      case "pass": check(exactKeys(outcome, ["kind"]), "pass fields"); break;
      case "fail": check(exactKeys(outcome, ["kind", "reason", "stack"]) && typeof outcome.reason === "string" && (outcome.stack === null || typeof outcome.stack === "string"), "failure diagnostics"); break;
      case "crash": check(exactKeys(outcome, ["kind", "panic"]) && typeof outcome.panic === "string", "crash diagnostics"); break;
      case "timeout": check(exactKeys(outcome, ["kind", "ms"]) && integer(outcome.ms), "timeout budget"); break;
      case "out_of_memory": check(exactKeys(outcome, ["kind", "bytes"]) && integer(outcome.bytes), "heap diagnostics"); break;
      case "skipped": check(exactKeys(outcome, ["kind", "reason"]), "skip fields"); skipReason(outcome.reason); break;
    }
    const own = sections[section(row.path)] ??= empty();
    for (const target of [totals, own]) { target.total++; target[buckets[outcome.kind]]++; }
    if (outcome.kind !== "pass" && outcome.kind !== "skipped") failures.push({ path: row.path, outcome: outcome.kind === "out_of_memory" ? "oom" : outcome.kind, reason: outcomeDetail(outcome), stack: outcome.stack ?? null });
  }
  check(totalsEqual(data.totals, totals), "total counts differ from rows");
  check(object(data.by_section) && Object.keys(data.by_section).sort().join("\0") === Object.keys(sections).sort().join("\0") && Object.entries(sections).every(([key, counts]) => totalsEqual(data.by_section[key], counts)), "section counts differ from rows");
  return { totals, by_section: sections, failures };
}

/** Read Node.js's separate corpus contract; it does not supply Test262 rows.
 * @param {any} data
 */
export function nodeModel(data) {
  check(object(data) && object(data.totals) && object(data.by_section) && Array.isArray(data.failing_tests), "Node.js aggregate fields required");
  check(keys.every(key => integer(data.totals[key])), "Node.js counts");
  check(data.failing_tests.every(row => object(row) && typeof row.path === "string" && typeof row.outcome === "string" && typeof row.reason === "string"), "Node.js failure rows");
  return { totals: data.totals, by_section: data.by_section, failures: data.failing_tests };
}

/** Choose an explicit corpus contract; no format probing or compatibility reader.
 * @param {string} corpus
 * @param {any} data
 */
export function conformanceModel(corpus, data) {
  if (corpus === "test262") return test262Model(data);
  if (corpus === "nodejs") return nodeModel(data);
  throw new Error(`Unknown conformance corpus: ${corpus}`);
}
