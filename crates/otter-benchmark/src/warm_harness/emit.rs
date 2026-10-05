//! One classic-Script clock, timing window and raw-record producer for all anchors.
//!
//! # Contents
//! - Captured hrtime.bigint and output helpers before benchmark globals.
//! - Untimed persistent setup/reset/validation and exact warm/measured records.
//!
//! # Invariants
//! - The original whole source is never wrapped in a function or a new realm.
//! - Only the complete fixed driver runs between measured timestamps.
//! - No result formatting, JSON, output or reset executes inside that window.
//!
//! # See also
//! - `super::records` strictly validates this one unversioned protocol.

use super::recipes::Plan;
use super::{WarmHarnessError, WarmHarnessRequest, WarmRecord, reject};

pub(super) const CLOCK: &str = "captured-process-hrtime-bigint";
pub(super) const SCOPE: &str = "complete-original-fixed-work; reset/outer-validation/format/output outside timer; original inner integrity checks included";
pub(super) const PREFIX: &str = "@@otter-warm ";

fn host_prefix() -> Result<String, WarmHarnessError> {
    let prefix = serde_json::to_string(PREFIX).map_err(|error| reject(error.to_string()))?;
    Ok(format!(
        r#"// Common persistent fixed-work Script. Original license/comments follow unchanged.
const __rfWarmHost=(function(){{
  if(typeof process!=='object' || typeof process.hrtime!=='function' || typeof process.hrtime.bigint!=='function')throw new Error('monotonic hrtime.bigint unavailable');
  const now=process.hrtime.bigint.bind(process.hrtime);
  const write=console.log.bind(console);
  const encode=JSON.stringify.bind(JSON);
  const first=now(),second=now();
  if(typeof first!=='bigint' || typeof second!=='bigint' || second<first)throw new Error('monotonic bigint clock required');
  return {{now:now,emit:function(record){{write({prefix}+encode(record));}}}};
}})();
var require=void 0,module=void 0,__dirname=void 0,window=void 0,importScripts=void 0;
if(typeof require!=='undefined' || typeof module!=='undefined' || typeof __dirname!=='undefined' || typeof window!=='undefined' || typeof importScripts!=='undefined')throw new Error('classic Script shell host bindings unavailable');
"#
    ))
}

pub(super) fn original(source: &str) -> Result<String, WarmHarnessError> {
    let mut script = host_prefix()?;
    script.push_str(source);
    super::ast::parse(&script, |_| Ok(()))?;
    Ok(script)
}

pub(super) fn emit(request: &WarmHarnessRequest, plan: &Plan) -> Result<String, WarmHarnessError> {
    let ready = WarmRecord::Ready {
        anchor: request.source.anchor,
        original_sha256: request.source.expected_sha256.clone(),
        scope: SCOPE.into(),
        clock: CLOCK.into(),
        warmup_count: request.sampling.warmup_count,
        sample_count: request.sampling.sample_count,
    };
    let ready = serde_json::to_string(&ready).map_err(|error| reject(error.to_string()))?;
    let anchor =
        serde_json::to_string(&request.source.anchor).map_err(|error| reject(error.to_string()))?;
    let mut output = host_prefix()?;
    output.push_str(&plan.setup);
    output.push('\n');
    output.push_str(&plan.saved);
    output.push_str("\nfunction __rfWarmReset(){\n");
    output.push_str(&plan.reset);
    output.push_str("\n}\n");
    output.push_str("function __rfWarmPrecheck(){\n");
    output.push_str(&plan.precheck);
    output.push_str("\n}\n");
    output.push_str("function __rfWarmWork(){\n");
    output.push_str(&plan.work);
    output.push_str("\n}\n");
    output.push_str("function __rfWarmCheck(){\n");
    output.push_str(&plan.check);
    output.push_str("\n}\n");
    output.push_str("function __rfWarmCleanup(){\n");
    output.push_str(&plan.cleanup);
    output.push_str("\n}\n");
    output.push_str(&format!(r#"
__rfWarmHost.emit({ready});
(function(){{
  for(let phase=0;phase<2;phase++){{
    const measured=phase===1;
    const count=measured?{samples}:{warmups};
    for(let index=0;index<count;index++){{
      __rfWarmReset();
      __rfWarmPrecheck();
      let start;
      if(measured)start=__rfWarmHost.now();
      __rfWarmWork();
      let end;
      if(measured)end=__rfWarmHost.now();
      if(measured && (typeof start!=='bigint' || typeof end!=='bigint' || end<=start))throw new Error('nonpositive or invalid elapsed clock');
      const observation=__rfWarmCheck();
      __rfWarmCleanup();
      __rfWarmHost.emit({{kind:'invocation',phase:measured?'measured':'warmup',index:index,
        elapsedNsDecimal:measured?(end-start).toString():null,result:observation.result,checks:observation.checks}});
    }}
  }}
}})();
{final_cleanup}
__rfWarmHost.emit({{kind:'complete',anchor:{anchor},warmupCount:{warmups},sampleCount:{samples}}});
void 0;
"#,warmups=request.sampling.warmup_count,samples=request.sampling.sample_count,final_cleanup=plan.final_cleanup));
    Ok(output)
}
