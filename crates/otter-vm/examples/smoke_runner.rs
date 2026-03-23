use otter_vm::RegisterValue;
use otter_vm::smoke::{default_cases, run_case};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for case in default_cases() {
        let result = run_case(&case)?;
        println!("{} => {}", case.name(), format_value(result.return_value()));
    }

    Ok(())
}

fn format_value(value: RegisterValue) -> String {
    if let Some(value) = value.as_i32() {
        return value.to_string();
    }
    if let Some(value) = value.as_bool() {
        return value.to_string();
    }
    if let Some(value) = value.as_number() {
        return value.to_string();
    }

    format!("raw({:#x})", value.raw_bits())
}
