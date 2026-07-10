//! Emit the checked active-opcode inventory as JSON.

fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&otter_bytecode::opcode_audit::opcode_inventory()).unwrap()
    );
}
