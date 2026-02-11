use std::process;

fn main() {
    if let Err(e) = ai_audit::cli::run() {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}
