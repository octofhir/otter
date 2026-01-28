use std::env;
use std::fs;

use otter_engine::Compiler;

fn main() {
    let mut file = None;
    let mut index: Option<usize> = None;
    let mut show_constants = false;
    let mut const_index: Option<usize> = None;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--file" => {
                file = args.next();
            }
            "--index" => {
                index = args.next().and_then(|v| v.parse::<usize>().ok());
            }
            "--constants" => {
                show_constants = true;
            }
            "--const-index" => {
                const_index = args.next().and_then(|v| v.parse::<usize>().ok());
            }
            _ => {}
        }
    }

    let Some(file) = file else {
        eprintln!("Usage: dump_module --file <path> [--index <n>]");
        std::process::exit(2);
    };

    let source = match fs::read_to_string(&file) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("Failed to read {}: {}", file, err);
            std::process::exit(1);
        }
    };

    let compiler = Compiler::new();
    let module = match compiler.compile(&source, &file) {
        Ok(m) => m,
        Err(err) => {
            eprintln!("Compile error: {}", err);
            std::process::exit(1);
        }
    };

    println!("module: {}", module.source_url);
    println!("function_count: {}", module.function_count());
    if let Some(idx) = const_index {
        if let Some(constant) = module.constants.get(idx as u32) {
            println!("constant[{}] = {:?}", idx, constant);
        } else {
            eprintln!("No constant at index {}", idx);
        }
    }

    if show_constants {
        println!("constants:");
        for (idx, constant) in module.constants.iter().enumerate() {
            println!("  {:04}: {:?}", idx, constant);
        }
    }

    if let Some(idx) = index {
        let Some(func) = module.function(idx as u32) else {
            eprintln!("No function at index {}", idx);
            std::process::exit(1);
        };
        println!("function[{}].name = {:?}", idx, func.name);
        println!("function[{}].param_count = {}", idx, func.param_count);
        println!("function[{}].locals = {}", idx, func.local_count);
        println!("function[{}].registers = {}", idx, func.register_count);
        println!("function[{}].is_generator = {}", idx, func.flags.is_generator);
        println!("function[{}].is_async = {}", idx, func.flags.is_async);
        println!("function[{}].is_arrow = {}", idx, func.flags.is_arrow);
        println!("instructions:");
        for (pc, insn) in func.instructions.iter().enumerate() {
            println!("{:04}: {:?}", pc, insn);
        }
    } else {
        for i in 0..module.function_count() {
            let func = module.function(i as u32).unwrap();
            println!("{:04} {:?}", i, func.name);
        }
    }
}
