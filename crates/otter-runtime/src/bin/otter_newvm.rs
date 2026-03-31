/// Minimal CLI for the new Otter VM.
/// Usage: otter-newvm <script.js>
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: otter-newvm <script.js>");
        std::process::exit(1);
    }
    let path = &args[1];
    let mut rt = otter_runtime::OtterRuntime::builder().build();
    match rt.run_file(path) {
        Ok(_) => {}
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
