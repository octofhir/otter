//! Static HTML conformance dashboard generator.
//!
//! Renders a [`Baseline`] report into one self-contained `index.html`
//! (inline CSS, no JavaScript, no external assets) so a full-corpus
//! run can be published as-is on any static host. The page shows the
//! headline pass-rate, per-group roll-ups as collapsible
//! `<details>` trees with stacked progress bars, and the failing
//! tests (with reasons) under each leaf section.
//!
//! # Contents
//!
//! - [`render_html`] — the only public entry point: `Baseline → String`.
//! - `Node` — section tree assembled from `by_section` keys
//!   (`built-ins/Array/prototype` → three nested levels) with totals
//!   aggregated bottom-up.
//!
//! # Invariants
//!
//! - Output is deterministic for a given baseline: the tree is a
//!   `BTreeMap` walk and failing tests keep report order.
//! - All test-supplied text (paths, failure reasons) is HTML-escaped.
//! - The page must stay dependency-free: everything inline, the
//!   collapse behaviour is native `<details>`/`<summary>`.
//!
//! # See also
//!
//! - [`crate::report`] — the `Baseline` wire format this consumes.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::report::{Baseline, FailingTest, Totals, section_of};

/// Section tree node: totals for this subtree + named children.
#[derive(Default)]
struct Node {
    totals: Totals,
    children: BTreeMap<String, Node>,
}

fn add_totals(into: &mut Totals, t: &Totals) {
    into.total += t.total;
    into.passed += t.passed;
    into.failed += t.failed;
    into.skipped += t.skipped;
    into.crashed += t.crashed;
    into.timed_out += t.timed_out;
    into.oom += t.oom;
}

fn build_tree(baseline: &Baseline) -> Node {
    let mut root = Node::default();
    for (section, t) in &baseline.by_section {
        add_totals(&mut root.totals, t);
        let mut node = &mut root;
        for seg in section.split('/') {
            node = node.children.entry(seg.to_string()).or_default();
            add_totals(&mut node.totals, t);
        }
    }
    root
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Stacked bar: green pass, red fail, orange crash/timeout/oom,
/// grey skip. Widths are percentages of `total`.
fn bar_html(t: &Totals) -> String {
    if t.total == 0 {
        return r#"<span class="bar"></span>"#.to_string();
    }
    let pct = |n: u64| (n as f64) * 100.0 / (t.total as f64);
    let broken = t.crashed + t.timed_out + t.oom;
    format!(
        r#"<span class="bar"><i class="p" style="width:{:.2}%"></i><i class="f" style="width:{:.2}%"></i><i class="b" style="width:{:.2}%"></i><i class="s" style="width:{:.2}%"></i></span>"#,
        pct(t.passed),
        pct(t.failed),
        pct(broken),
        pct(t.skipped),
    )
}

fn counts_html(t: &Totals) -> String {
    format!(
        r#"<span class="counts">{} / {} <em>({:.2}%)</em></span>"#,
        t.passed,
        t.total,
        t.pass_rate()
    )
}

fn outcome_class(outcome: &str) -> &'static str {
    match outcome {
        "crash" => "o-crash",
        "timeout" => "o-timeout",
        "oom" => "o-oom",
        _ => "o-fail",
    }
}

fn render_failures(out: &mut String, rows: &[&FailingTest]) {
    out.push_str(r#"<table class="fails"><thead><tr><th>Outcome</th><th>Test</th><th>Reason</th></tr></thead><tbody>"#);
    for row in rows {
        let _ = write!(
            out,
            r#"<tr><td><span class="badge {}">{}</span></td><td><code>{}</code></td><td class="reason">{}</td></tr>"#,
            outcome_class(&row.outcome),
            escape(&row.outcome),
            escape(&row.path),
            escape(&row.reason),
        );
    }
    out.push_str("</tbody></table>");
}

fn render_node(
    out: &mut String,
    name: &str,
    path: &str,
    node: &Node,
    failures: &BTreeMap<&str, Vec<&FailingTest>>,
    depth: usize,
) {
    let leaf_failures = failures.get(path).map(Vec::as_slice).unwrap_or(&[]);
    let has_body = !node.children.is_empty() || !leaf_failures.is_empty();
    let row = format!(
        r#"<span class="name">{}</span>{}{}"#,
        escape(name),
        bar_html(&node.totals),
        counts_html(&node.totals),
    );
    if !has_body {
        let _ = write!(out, r#"<div class="row leaf">{row}</div>"#);
        return;
    }
    // Top-level groups start open so the page is scannable at a glance.
    let open = if depth == 0 { " open" } else { "" };
    let _ = write!(
        out,
        r#"<details{open}><summary class="row">{row}</summary><div class="kids">"#
    );
    for (child_name, child) in &node.children {
        let child_path = if path.is_empty() {
            child_name.clone()
        } else {
            format!("{path}/{child_name}")
        };
        render_node(out, child_name, &child_path, child, failures, depth + 1);
    }
    if !leaf_failures.is_empty() {
        render_failures(out, leaf_failures);
    }
    out.push_str("</div></details>");
}

/// Render the whole dashboard page.
#[must_use]
pub fn render_html(baseline: &Baseline) -> String {
    let root = build_tree(baseline);

    // Group failing tests under their section so leaves can list them.
    let mut failures: BTreeMap<&str, Vec<&FailingTest>> = BTreeMap::new();
    for row in &baseline.failing_tests {
        failures.entry(section_of(&row.path)).or_default().push(row);
    }

    let t = &baseline.totals;
    let mut out = String::with_capacity(1 << 20);
    out.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    out.push_str("<title>Otter \u{2014} Test262 Conformance</title>\n<style>\n");
    out.push_str(STYLE);
    out.push_str("</style>\n</head>\n<body>\n");

    let _ = write!(
        out,
        r#"<header><h1>Otter — ECMAScript Conformance</h1>
<p class="meta">Engine <code>{}</code> · test262 <code>{}</code> · {}</p>
<div class="hero"><span class="rate">{:.2}%</span>{}</div>
<p class="legend"><span class="chip c-pass"></span>pass {} <span class="chip c-fail"></span>fail {} <span class="chip c-broken"></span>crash {} / timeout {} / oom {} <span class="chip c-skip"></span>skip {} · total {}</p>
</header><main>"#,
        escape(&short_sha(&baseline.engine_commit)),
        escape(&short_sha(&baseline.test262_commit)),
        escape(&baseline.ran_at),
        t.pass_rate(),
        bar_html(t),
        t.passed,
        t.failed,
        t.crashed,
        t.timed_out,
        t.oom,
        t.skipped,
        t.total,
    );

    for (name, node) in &root.children {
        render_node(&mut out, name, name, node, &failures, 0);
    }

    out.push_str("</main>\n</body>\n</html>\n");
    out
}

fn short_sha(s: &str) -> String {
    if s.len() > 12 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        s[..12].to_string()
    } else {
        s.to_string()
    }
}

const STYLE: &str = r#"
:root{--pass:#2da44e;--fail:#cf222e;--broken:#bf8700;--skip:#afb8c1;--ink:#1f2328;--sub:#57606a;--line:#d0d7de;--bg:#ffffff;--bg2:#f6f8fa}
*{box-sizing:border-box}
body{margin:0;font:14px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",Helvetica,Arial,sans-serif;color:var(--ink);background:var(--bg)}
header{padding:24px 32px;border-bottom:1px solid var(--line);background:var(--bg2)}
h1{margin:0 0 4px;font-size:22px}
.meta{margin:0 0 16px;color:var(--sub)}
.meta code,main code{font:12px ui-monospace,SFMono-Regular,Menlo,monospace;background:rgba(175,184,193,.2);padding:1px 4px;border-radius:4px}
.hero{display:flex;align-items:center;gap:16px}
.hero .rate{font-size:34px;font-weight:600}
.hero .bar{height:18px;flex:1}
.legend{margin:10px 0 0;color:var(--sub)}
.chip{display:inline-block;width:10px;height:10px;border-radius:2px;margin:0 4px 0 12px}
.chip.c-pass{background:var(--pass)}.chip.c-fail{background:var(--fail)}.chip.c-broken{background:var(--broken)}.chip.c-skip{background:var(--skip)}
main{padding:16px 32px;max-width:1100px}
.bar{display:inline-flex;width:240px;height:10px;border-radius:5px;overflow:hidden;background:var(--bg2);border:1px solid var(--line);vertical-align:middle}
.bar i{display:block;height:100%}
.bar .p{background:var(--pass)}.bar .f{background:var(--fail)}.bar .b{background:var(--broken)}.bar .s{background:var(--skip)}
.row{display:flex;align-items:center;gap:12px;padding:4px 0}
.row .name{flex:0 0 auto;min-width:220px}
.row .counts{color:var(--sub);white-space:nowrap}
.row .counts em{font-style:normal}
details{border-left:1px solid var(--line);padding-left:0}
details>summary{cursor:pointer;list-style-position:inside;padding-left:4px}
details>summary:hover{background:var(--bg2)}
.kids{margin-left:24px}
.leaf{padding-left:22px}
.fails{border-collapse:collapse;margin:8px 0 16px;width:100%}
.fails th,.fails td{border:1px solid var(--line);padding:4px 8px;text-align:left;vertical-align:top}
.fails th{background:var(--bg2)}
.fails .reason{color:var(--sub);font-size:13px;word-break:break-word}
.badge{display:inline-block;padding:1px 7px;border-radius:10px;font-size:12px;color:#fff}
.badge.o-fail{background:var(--fail)}.badge.o-crash{background:#8250df}.badge.o-timeout{background:var(--broken)}.badge.o-oom{background:#0969da}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Baseline;

    fn synth() -> Baseline {
        let json = r#"{
            "test262_commit": "abc",
            "engine_commit": "def",
            "ran_at": "2026-06-04T00:00:00Z",
            "totals": {"total": 3, "passed": 1, "failed": 1, "skipped": 1, "crashed": 0, "timed_out": 0, "oom": 0},
            "by_section": {
                "built-ins/Math/abs": {"total": 2, "passed": 1, "failed": 1, "skipped": 0, "crashed": 0, "timed_out": 0, "oom": 0},
                "language/expressions/addition": {"total": 1, "passed": 0, "failed": 0, "skipped": 1, "crashed": 0, "timed_out": 0, "oom": 0}
            },
            "failing_tests": [
                {"path": "built-ins/Math/abs/nan.js", "outcome": "fail", "reason": "expected <b>NaN</b>"}
            ]
        }"#;
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn renders_tree_and_escapes_reasons() {
        let html = render_html(&synth());
        assert!(html.contains("built-ins"));
        assert!(html.contains("Math"));
        assert!(html.contains("abs"));
        assert!(html.contains("expected &lt;b&gt;NaN&lt;/b&gt;"));
        assert!(!html.contains("expected <b>NaN</b>"));
    }

    #[test]
    fn totals_aggregate_to_top_groups() {
        let html = render_html(&synth());
        // built-ins group rolls up 1/2 (50.00%).
        assert!(html.contains("1 / 2 <em>(50.00%)</em>"));
    }

    #[test]
    fn page_is_self_contained() {
        let html = render_html(&synth());
        assert!(!html.contains("<script"));
        assert!(!html.contains("http://"));
        assert!(!html.contains("https://"));
    }
}
