fn main() {
    if let Err(err) = oxdock::run() {
        eprintln!("{err:#}");
        std::process::exit(oxdock::process_exit_code(&err));
    }
}
