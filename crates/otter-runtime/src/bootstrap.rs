use crate::JscResult;
use crate::context::JscContext;

const BOOTSTRAP_JS: &str = include_str!("bootstrap.js");

pub fn register_bootstrap(ctx: &JscContext) -> JscResult<()> {
    // First, set up the node builtins list (dynamic, small)
    let node_builtins_json = serde_json::to_string(crate::NODE_BUILTINS)
        .expect("NODE_BUILTINS should serialize to JSON");
    let setup_script = format!(
        "globalThis.__otter_node_builtin_names = {};",
        node_builtins_json
    );
    ctx.eval_with_source(&setup_script, "<otter_bootstrap_setup>")?;

    ctx.eval_with_source(BOOTSTRAP_JS, "<otter_bootstrap>")?;

    Ok(())
}
