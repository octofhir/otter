//! Interactive HTML conformance dashboard generator.
//!
//! Renders a [`Baseline`] report into one self-contained `index.html`
//! (inline CSS + vanilla JS, no external assets) so a full-corpus run
//! can be published as-is on any static host — including the
//! contributor book (`docs/book/src/conformance/`). The page embeds
//! the report as JSON and renders client-side: headline pass-rate,
//! drill-down group tree with stacked progress bars, per-section
//! failing-test tables with reasons, live path filter, and links from
//! each failing test to its source in the pinned test262 commit.
//!
//! # Contents
//!
//! - [`render_html`] — the only public entry point: `Baseline → String`.
//!
//! # Invariants
//!
//! - Output is deterministic for a given baseline (BTreeMap iteration
//!   order; failing tests keep report order).
//! - The embedded JSON is `</`-escaped so report text can never break
//!   out of the `<script>` data island.
//! - The page must stay dependency-free: inline CSS/JS only, no
//!   external fetches.
//!
//! # See also
//!
//! - [`crate::report`] — the `Baseline` wire format this consumes.

use std::fmt::Write as _;

use crate::report::Baseline;

/// Render the whole dashboard page.
#[must_use]
pub fn render_html(baseline: &Baseline) -> String {
    let json = serde_json::to_string(baseline)
        .unwrap_or_else(|_| "{}".to_string())
        // Keep report text from terminating the data island.
        .replace("</", "<\\/");

    let mut out = String::with_capacity(json.len() + (STYLE.len() + SCRIPT.len()) * 2);
    out.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    out.push_str("<title>Otter — ECMAScript Conformance</title>\n<style>");
    out.push_str(STYLE);
    out.push_str("</style>\n</head>\n<body>\n");
    out.push_str(
        "<header><h1>Otter — ECMAScript Conformance</h1><div id=\"hero\"></div></header>\n",
    );
    out.push_str("<main><input id=\"filter\" type=\"search\" placeholder=\"Filter sections… (e.g. built-ins/Array)\" autocomplete=\"off\"><div id=\"tree\"></div></main>\n");
    let _ = write!(
        out,
        "<script id=\"data\" type=\"application/json\">{json}</script>\n<script>"
    );
    out.push_str(SCRIPT);
    out.push_str("</script>\n</body>\n</html>\n");
    out
}

const STYLE: &str = r#"
:root{--pass:#2da44e;--fail:#cf222e;--broken:#bf8700;--skip:#afb8c1;--ink:#1f2328;--sub:#57606a;--line:#d0d7de;--bg:#ffffff;--bg2:#f6f8fa;--accent:#0969da}
*{box-sizing:border-box}
body{margin:0;font:14px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",Helvetica,Arial,sans-serif;color:var(--ink);background:var(--bg)}
header{padding:24px 32px;border-bottom:1px solid var(--line);background:var(--bg2)}
h1{margin:0 0 12px;font-size:22px}
code{font:12px ui-monospace,SFMono-Regular,Menlo,monospace;background:rgba(175,184,193,.2);padding:1px 4px;border-radius:4px}
.meta{margin:0 0 16px;color:var(--sub)}
.hero-row{display:flex;align-items:center;gap:16px}
.rate{font-size:34px;font-weight:600}
.hero-row .bar{height:18px;flex:1}
.legend{margin:10px 0 0;color:var(--sub)}
.chip{display:inline-block;width:10px;height:10px;border-radius:2px;margin:0 4px 0 12px}
.chip.c-pass{background:var(--pass)}.chip.c-fail{background:var(--fail)}.chip.c-broken{background:var(--broken)}.chip.c-skip{background:var(--skip)}
main{padding:16px 32px;max-width:1200px}
#filter{width:100%;max-width:480px;margin:0 0 16px;padding:6px 10px;font:inherit;border:1px solid var(--line);border-radius:6px}
.bar{display:inline-flex;width:220px;height:10px;border-radius:5px;overflow:hidden;background:var(--bg2);border:1px solid var(--line);flex:none}
.bar i{display:block;height:100%}
.bar .p{background:var(--pass)}.bar .f{background:var(--fail)}.bar .b{background:var(--broken)}.bar .s{background:var(--skip)}
.node{margin:0}
.row{display:flex;align-items:center;gap:12px;padding:5px 8px;border-radius:6px;cursor:pointer;user-select:none}
.row:hover{background:var(--bg2)}
.row .tw{flex:none;width:14px;color:var(--sub);font-size:11px;transition:transform .12s}
.row.open .tw{transform:rotate(90deg)}
.row .name{flex:0 0 auto;min-width:260px;font-weight:500}
.row .pct{color:var(--sub);width:72px;text-align:right;font-variant-numeric:tabular-nums}
.row .counts{color:var(--sub);white-space:nowrap;font-variant-numeric:tabular-nums}
.row.leafrow{cursor:default}
.row.haserr{cursor:pointer}
.kids{margin-left:22px;border-left:1px solid var(--line);padding-left:6px;display:none}
.kids.open{display:block}
.fails{border-collapse:collapse;margin:6px 0 14px;width:100%}
.fails th,.fails td{border:1px solid var(--line);padding:4px 8px;text-align:left;vertical-align:top}
.fails th{background:var(--bg2)}
.fails .reason{color:var(--sub);font-size:13px;word-break:break-word}
.fails a{color:var(--accent);text-decoration:none}
.fails a:hover{text-decoration:underline}
.badge{display:inline-block;padding:1px 7px;border-radius:10px;font-size:12px;color:#fff}
.badge.o-fail{background:var(--fail)}.badge.o-crash{background:#8250df}.badge.o-timeout{background:var(--broken)}.badge.o-oom{background:var(--accent)}
.empty{color:var(--sub);padding:8px}
"#;

const SCRIPT: &str = r#"
"use strict";
const DATA = JSON.parse(document.getElementById("data").textContent);

// ---- model -----------------------------------------------------------
// Build the section tree from by_section keys ("built-ins/Array/from").
function emptyTotals(){return {total:0,passed:0,failed:0,skipped:0,crashed:0,timed_out:0,oom:0};}
function addTotals(a,b){for(const k of Object.keys(a))a[k]+=b[k]||0;}
const root={name:"",totals:emptyTotals(),kids:new Map()};
for(const [section,t] of Object.entries(DATA.by_section||{})){
  addTotals(root.totals,t);
  let node=root;
  for(const seg of section.split("/")){
    if(!node.kids.has(seg))node.kids.set(seg,{name:seg,totals:emptyTotals(),kids:new Map()});
    node=node.kids.get(seg);
    addTotals(node.totals,t);
  }
}
// Failing tests grouped by their (three-segment) section.
const fails=new Map();
for(const f of DATA.failing_tests||[]){
  const seg=f.path.split("/");
  const key=seg.slice(0,Math.min(3,seg.length)).join("/");
  if(!fails.has(key))fails.set(key,[]);
  fails.get(key).push(f);
}
function passRate(t){const d=t.total-t.skipped;return d>0?t.passed*100/d:0;}

// ---- rendering -------------------------------------------------------
function el(tag,cls,text){const e=document.createElement(tag);if(cls)e.className=cls;if(text!==undefined)e.textContent=text;return e;}
function bar(t){
  const b=el("span","bar");
  const mk=(cls,n)=>{const i=el("i",cls);i.style.width=(t.total?n*100/t.total:0)+"%";b.appendChild(i);};
  mk("p",t.passed);mk("f",t.failed);mk("b",t.crashed+t.timed_out+t.oom);mk("s",t.skipped);
  return b;
}
function testUrl(path){
  const c=DATA.test262_commit&&DATA.test262_commit!=="unknown"?DATA.test262_commit:"main";
  return "https://github.com/tc39/test262/blob/"+c+"/test/"+path;
}
function failTable(rows){
  const tb=el("table","fails");
  tb.innerHTML="<thead><tr><th>Outcome</th><th>Test</th><th>Reason</th></tr></thead>";
  const body=el("tbody");
  for(const f of rows){
    const tr=el("tr");
    const td1=el("td");td1.appendChild(el("span","badge o-"+f.outcome,f.outcome));tr.appendChild(td1);
    const td2=el("td");const a=el("a");a.href=testUrl(f.path);a.target="_blank";a.rel="noopener";
    const cd=el("code",null,f.path);a.appendChild(cd);td2.appendChild(a);tr.appendChild(td2);
    tr.appendChild(el("td","reason",f.reason));
    body.appendChild(tr);
  }
  tb.appendChild(body);
  return tb;
}
function renderNode(node,path,depth){
  const wrap=el("div","node");
  wrap.dataset.path=path;
  const leafFails=fails.get(path)||[];
  const hasKids=node.kids.size>0;
  const expandable=hasKids||leafFails.length>0;
  const row=el("div","row"+(expandable?" haserr":" leafrow"));
  row.appendChild(el("span","tw",expandable?"▶":""));
  row.appendChild(el("span","name",node.name));
  row.appendChild(bar(node.totals));
  row.appendChild(el("span","pct",passRate(node.totals).toFixed(1)+"%"));
  row.appendChild(el("span","counts",node.totals.passed+" / "+node.totals.total
    +(node.totals.failed?"  ·  "+node.totals.failed+" fail":"")
    +(node.totals.skipped?"  ·  "+node.totals.skipped+" skip":"")));
  wrap.appendChild(row);
  if(expandable){
    const kids=el("div","kids");
    let built=false;
    row.addEventListener("click",()=>{
      if(!built){ // lazy render on first expand
        for(const [name,child] of [...node.kids.entries()].sort((a,b)=>a[0]<b[0]?-1:1)){
          kids.appendChild(renderNode(child,path?path+"/"+name:name,depth+1));
        }
        if(leafFails.length)kids.appendChild(failTable(leafFails));
        built=true;
      }
      const open=kids.classList.toggle("open");
      row.classList.toggle("open",open);
    });
    wrap.appendChild(kids);
  }
  return wrap;
}
function renderTree(filter){
  const tree=document.getElementById("tree");
  tree.textContent="";
  const names=[...root.kids.keys()].sort();
  let shown=0;
  for(const name of names){
    if(filter){
      // Show a top-level group when any of its sections matches.
      const any=Object.keys(DATA.by_section||{}).some(s=>s.startsWith(name)&&s.includes(filter));
      if(!any)continue;
    }
    tree.appendChild(renderNode(root.kids.get(name),name,0));
    shown++;
  }
  if(!shown)tree.appendChild(el("div","empty","No sections match the filter."));
}

// ---- header ----------------------------------------------------------
(function(){
  const t=DATA.totals||emptyTotals();
  const hero=document.getElementById("hero");
  const meta=el("p","meta");
  meta.innerHTML="Engine <code>"+String(DATA.engine_commit||"?").slice(0,12)
    +"</code> · test262 <code>"+String(DATA.test262_commit||"?").slice(0,12)
    +"</code> · "+(DATA.ran_at||"");
  hero.appendChild(meta);
  const row=el("div","hero-row");
  row.appendChild(el("span","rate",passRate(t).toFixed(2)+"%"));
  row.appendChild(bar(t));
  hero.appendChild(row);
  const legend=el("p","legend");
  legend.innerHTML='<span class="chip c-pass"></span>pass '+t.passed
    +' <span class="chip c-fail"></span>fail '+t.failed
    +' <span class="chip c-broken"></span>crash '+t.crashed+" / timeout "+t.timed_out+" / oom "+t.oom
    +' <span class="chip c-skip"></span>skip '+t.skipped+" · total "+t.total;
  hero.appendChild(legend);
})();

let debounce=null;
document.getElementById("filter").addEventListener("input",e=>{
  clearTimeout(debounce);
  debounce=setTimeout(()=>renderTree(e.target.value.trim()),120);
});
renderTree("");
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
                {"path": "built-ins/Math/abs/nan.js", "outcome": "fail", "reason": "expected </script><b>NaN</b>"}
            ]
        }"#;
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn embeds_report_as_escaped_data_island() {
        let html = render_html(&synth());
        // Data island present and `</` is escaped so the reason text
        // cannot terminate the script element.
        assert!(html.contains(r#"<script id="data" type="application/json">"#));
        assert!(html.contains(r"<\/script><b>NaN<\/b>"));
        assert!(!html.contains("expected </script>"));
    }

    #[test]
    fn page_is_self_contained() {
        let html = render_html(&synth());
        assert!(!html.contains("src=\"http"));
        assert!(!html.contains("href=\"http"));
        assert!(!html.contains("@import"));
        assert!(!html.contains("fetch("));
    }

    #[test]
    fn sections_and_totals_reach_the_client_model() {
        let html = render_html(&synth());
        assert!(html.contains("built-ins/Math/abs"));
        assert!(html.contains("\"passed\":1"));
    }
}
