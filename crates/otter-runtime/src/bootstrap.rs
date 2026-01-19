use crate::JscResult;
use crate::context::JscContext;

const BOOTSTRAP_JS: &str = include_str!("bootstrap.js");

pub fn register_bootstrap(ctx: &JscContext) -> JscResult<()> {
    let node_builtins_json = serde_json::to_string(crate::NODE_BUILTINS)
        .expect("NODE_BUILTINS should serialize to JSON");
    let script = format!(
        "globalThis.__otter_node_builtin_names = {};\n{}",
        node_builtins_json, BOOTSTRAP_JS
    );
    ctx.eval_with_source(&script, "<otter_bootstrap>")?;
    Ok(())
}
